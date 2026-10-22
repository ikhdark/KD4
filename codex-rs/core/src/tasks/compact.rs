use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use super::emit_compact_metric;
use crate::session::TurnInput;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
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
            let result = if crate::compact::should_use_remote_compact_task(
                ctx.provider.info(),
                ctx.config.compact_prompt.as_deref(),
            ) {
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
            // Preparation and durable publication can fail without emitting an
            // Error event. The terminal owner must receive every returned error.
            result?;
            Ok(super::TurnTaskResult::default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compact_task_returns_recovery_storage_failure_and_keeps_history() -> anyhow::Result<()>
    {
        use crate::test_harness::responses;
        use codex_protocol::error::CodexErr;
        use codex_protocol::models::ResponseItem;
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                    config.compact_prompt = Some("Summarize this task".into());
                },
            )
            .await;
        let input: ResponseItem = serde_json::from_value(serde_json::json!({
            "type":"message", "role":"user", "content":[{
                "type":"input_text", "text":"Keep exact unresolved constraints. ".repeat(20_000)
            }]
        }))?;
        session.record_conversation_items(&turn, &[input]).await?;
        let before = session.clone_history().await;
        let window = session.current_window_id().await;
        // Exercise the real recovery-artifact writer, not a synthetic task result.
        std::fs::write(home.path().join("tool-output"), "not a directory")?;
        let log = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("summary", "Summary fixture"),
                responses::ev_completed("summary-response"),
            ]),
        )
        .await;
        let result = Arc::new(CompactTask)
            .run(
                Arc::clone(&session),
                turn,
                Vec::new(),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(CodexErr::Fatal(_))));
        assert_eq!(session.current_window_id().await, window);
        assert_eq!(
            session.clone_history().await.raw_items(),
            before.raw_items()
        );
        assert!(
            log.requests().len() <= 1,
            "storage failure must not regenerate a summary"
        );
        Ok(())
    }
}
