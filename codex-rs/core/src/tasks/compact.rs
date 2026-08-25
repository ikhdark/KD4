use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use super::emit_compact_metric;
use crate::session::TurnInput;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::error::CodexErr;
use codex_protocol::user_input::UserInput;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactTask;

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            let result = if crate::compact::should_use_remote_compact_task(ctx.provider.info()) {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "remote_v2",
                    /*manual*/ true,
                );
                crate::compact_remote_v2::run_remote_compact_task(
                    session.clone(),
                    ctx,
                    &cancellation_token,
                )
                .await
            } else {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "local",
                    /*manual*/ true,
                );
                let input = vec![UserInput::Text {
                    text: ctx
                        .config
                        .compact_prompt
                        .as_deref()
                        .unwrap_or(crate::compact::SUMMARIZATION_PROMPT)
                        .to_string(),
                    // Compaction prompt is synthesized; no UI element ranges to preserve.
                    text_elements: Vec::new(),
                }];
                crate::compact::run_compact_task(session.clone(), ctx, input, &cancellation_token)
                    .await
            };
            if let Err(err @ CodexErr::TurnAborted) = result {
                return Err(err);
            }
            Ok(super::TurnTaskResult::default())
        })
    }
}
