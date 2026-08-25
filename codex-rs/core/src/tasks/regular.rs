use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskResult;

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
            sess.set_server_reasoning_included(/*included*/ false).await;
            let mut next_input = input;
            loop {
                let last_agent_message = run_turn(
                    Arc::clone(&sess),
                    Arc::clone(&ctx),
                    Arc::clone(&turn_extension_data),
                    next_input,
                    None,
                    cancellation_token.child_token(),
                )
                .instrument(run_turn_span.clone())
                .await?;
                if !sess.input_queue.has_pending_input(&sess.active_turn).await {
                    return Ok(last_agent_message);
                }
                next_input = Vec::new();
            }
        })
    }
}
