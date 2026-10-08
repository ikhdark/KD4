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
    pub(super) retention_input: Vec<ResponseItem>,
    pub(super) compaction_output: ResponseItem,
    pub(super) token_usage: Option<TokenUsage>,
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
    let tool_router = built_tools(sess.as_ref(), step_context, &[], cancellation_token).await?;
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let mut prepared = prepare_sampling_prompt_for_client(
        history.clone(),
        turn_context,
        sess.services.git_workspace.as_ref(),
    )
    .await;
    let mut prompt = build_projected_prompt(
        sess.as_ref(),
        &prepared,
        &tool_router,
        step_context.as_ref(),
        base_instructions.clone(),
    );
    prompt.output_schema = None;
    prompt.output_schema_strict = true;
    append_compaction_trigger(&mut prompt);
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
        prepared = prepare_sampling_prompt_for_client(
            history.clone(),
            turn_context,
            sess.services.git_workspace.as_ref(),
        )
        .await;
        prompt = build_projected_prompt(
            sess.as_ref(),
            &prepared,
            &tool_router,
            step_context.as_ref(),
            base_instructions,
        );
        prompt.output_schema = None;
        prompt.output_schema_strict = true;
        append_compaction_trigger(&mut prompt);
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
    // Sampling projection may split messages and discard their trusted IDs.
    // Select exact task/skill retention from the same source history instead;
    // it also excludes the synthetic compaction trigger by construction.
    let retention_input = history.into_raw_items();
    Ok(RemoteCompactV2Attempt {
        trace_input_history,
        retention_input,
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
        let tokens = measured.iter().find(|(previous, _)| Arc::ptr_eq(previous, input))
            .map(|(_, tokens)| *tokens)
            .unwrap_or_else(|| input.iter()
                .map(crate::context_manager::estimate_item_token_count)
                .fold(0_i64, i64::saturating_add));
        measured.push((input, tokens));
    }
    measured.into_iter().max_by_key(|(_, tokens)| *tokens)
        .map(|(input, _)| Arc::clone(input))
        .unwrap_or_else(|| Arc::clone(&prompt.input))
}

fn append_compaction_trigger(prompt: &mut Prompt) {
    // Preserve sharing established by prompt projection. Pointer identity is
    // sufficient; do not compare large distinct transcripts for equality.
    let mut appended: Vec<(Arc<[ResponseItem]>, Arc<[ResponseItem]>)> = Vec::with_capacity(4);
    for input in [
        &mut prompt.input,
        &mut prompt.stable_context_fallback_input,
        &mut prompt.tool_history_fallback_input,
        &mut prompt.stable_context_tool_history_fallback_input,
    ] {
        if let Some((_, replacement)) = appended.iter()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_variants_preserve_sharing_and_measure_distinct_fallbacks() {
        let small: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None, encrypted_content: "x".repeat(1_000),
            internal_chat_message_metadata_passthrough: None,
        }].into();
        let large: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None, encrypted_content: "y".repeat(10_000),
            internal_chat_message_metadata_passthrough: None,
        }].into();
        let mut prompt = Prompt {
            input: Arc::clone(&small),
            stable_context_fallback_input: Arc::clone(&small),
            tool_history_fallback_input: Arc::clone(&large),
            stable_context_tool_history_fallback_input: Arc::clone(&large),
            ..Default::default()
        };
        append_compaction_trigger(&mut prompt);
        assert_eq!(small.len(), 1);
        assert_eq!(large.len(), 1);
        assert!(Arc::ptr_eq(&prompt.input, &prompt.stable_context_fallback_input));
        assert!(Arc::ptr_eq(&prompt.tool_history_fallback_input,
            &prompt.stable_context_tool_history_fallback_input));
        assert!(!Arc::ptr_eq(&prompt.input, &prompt.tool_history_fallback_input));
        for input in [&prompt.input, &prompt.stable_context_fallback_input,
            &prompt.tool_history_fallback_input, &prompt.stable_context_tool_history_fallback_input] {
            assert_eq!(input.len(), 2);
            assert!(matches!(input.last(), Some(ResponseItem::CompactionTrigger {})));
        }
        assert!(Arc::ptr_eq(&largest_compaction_request_input(&prompt),
            &prompt.stable_context_tool_history_fallback_input));
    }

    #[test]
    #[ignore]
    fn benchmark_compaction_shared_variants() {
        let input: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None,
            encrypted_content: "x".repeat(2_000_000),
            internal_chat_message_metadata_passthrough: None,
        }].into();
        let mut samples = Vec::new();
        for _ in 0..25 {
            let mut prompt = Prompt {
                input: Arc::clone(&input),
                stable_context_fallback_input: Arc::clone(&input),
                tool_history_fallback_input: Arc::clone(&input),
                stable_context_tool_history_fallback_input: Arc::clone(&input),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            append_compaction_trigger(&mut prompt);
            std::hint::black_box(largest_compaction_request_input(&prompt));
            samples.push(started.elapsed().as_micros());
        }
        samples.sort_unstable();
        eprintln!("compaction_shared_variants_us={samples:?} median={}", samples[12]);
        if let Some(directory) = std::env::var_os("COMPACTION_BENCHMARK_DIR") {
            std::fs::write(std::path::PathBuf::from(directory).join("shared-variants.txt"),
                format!("microseconds={samples:?}\nmedian={}\n", samples[12])).unwrap();
        }
    }
}
