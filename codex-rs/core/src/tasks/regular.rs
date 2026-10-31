use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::turn::LogicalGenerationBudget;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskResult;
use super::TurnTaskResult;

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            let sess = session;
            let turn_extension_data = Arc::clone(&ctx.extension_data);
            let run_turn_span = trace_span!("run_turn");
            // Emit the turn lifecycle immediately. Startup prewarm ownership is claimed only at the
            // first model-send boundary, after ordinary turn preparation has completed.
            let event = EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: ctx.sub_id.clone(),
                trace_id: ctx.trace_id.clone(),
                started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                model_context_window: ctx.model_context_window(),
                collaboration_mode_kind: ctx.collaboration_mode.mode,
            });
            sess.send_event(ctx.as_ref(), event).await;
            let mut next_input = input;
            let mut logical_generation_budget = LogicalGenerationBudget::default();
            loop {
                let turn_result = Box::pin(run_turn(
                    Arc::clone(&sess),
                    Arc::clone(&ctx),
                    Arc::clone(&turn_extension_data),
                    next_input,
                    None,
                    &mut logical_generation_budget,
                    cancellation_token.child_token(),
                ))
                .instrument(run_turn_span.clone())
                .await?;
                if !should_reenter_regular_task(
                    &turn_result,
                    sess.input_queue.has_pending_input(&sess.active_turn),
                )
                .await
                {
                    // Hooks and pending-input admission have selected the final owner.
                    // Surfaced candidates already passed this exact prepared contract;
                    // ordinary (including absent) finals must satisfy it too. A turn that
                    // already recorded a terminal error has no final; keep that cause.
                    validate_final_response(&ctx, &turn_result).await?;
                    // Completion metadata is not a transcript item. Publish direct
                    // answers through the normal persisted message lifecycle so
                    // clients see them both live and when reopening the thread.
                    if let Some(message) = turn_result
                        .surfaced_result
                        .as_ref()
                        .and_then(|result| result.canonical_message.as_ref())
                    {
                        ctx.turn_timing_state.record_direct_final_content();
                        sess.record_response_item_and_emit_turn_item(
                            &ctx,
                            ResponseItem::Message {
                                id: Some(ResponseItemId::new("msg")),
                                role: "assistant".to_string(),
                                content: vec![ContentItem::OutputText {
                                    text: message.clone(),
                                }],
                                phase: Some(MessagePhase::FinalAnswer),
                                internal_chat_message_metadata_passthrough: None,
                            },
                        )
                        .await?;
                    }
                    return Ok(turn_result);
                }
                next_input = Vec::new();
            }
        })
    }
}

async fn validate_final_response(
    ctx: &TurnContext,
    result: &TurnTaskResult,
) -> codex_protocol::error::Result<()> {
    if result.required_tool_terminal.is_none()
        && ctx.post_tool_stop_reason.get().is_none()
        && let Some(schema) = ctx.final_output_json_schema.as_ref()
        && ctx.terminal_error.lock().await.is_none()
    {
        schema
            .validate_text(
                result
                    .surfaced_result
                    .as_ref()
                    .and_then(|result| result.canonical_message.as_deref())
                    .or_else(|| {
                        result
                            .accepted_final_message
                            .as_ref()
                            .map(|message| message.text.as_str())
                    })
                    .or(result.last_agent_message.as_deref()),
            )
            .map_err(|error| ctx.final_response_validation_error(error))?;
    }
    Ok(())
}

async fn should_reenter_regular_task<F>(turn_result: &TurnTaskResult, has_pending_input: F) -> bool
where
    F: Future<Output = bool>,
{
    if turn_result.required_tool_terminal.is_some() || turn_result.defer_pending_input {
        false
    } else {
        has_pending_input.await
    }
}

