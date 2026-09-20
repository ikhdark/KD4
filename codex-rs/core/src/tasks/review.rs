use std::fmt::Write as _;
use std::sync::Arc;

use codex_prompts::render_review_exit_interrupted;
use codex_prompts::render_review_exit_success;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::ExitedReviewModeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::review_format::format_review_findings_block;
use codex_protocol::review_format::render_review_output_text;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::run_codex_thread_one_shot;
use crate::config::Constrained;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_features::Feature;
use codex_protocol::user_input::UserInput;

use super::SessionTask;
use super::SessionTaskResult;

#[derive(Clone)]
pub(crate) struct ReviewTask {
    entered_review_mode: EnteredReviewModeItem,
}

impl ReviewTask {
    pub(crate) fn new(entered_review_mode: EnteredReviewModeItem) -> Self {
        Self {
            entered_review_mode,
        }
    }
}

impl SessionTask for ReviewTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Review
    }

    fn span_name(&self) -> &'static str {
        "session_task.review"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            let sess = Arc::clone(&session);
            let start_event = EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: ctx.sub_id.clone(),
                trace_id: ctx.trace_id.clone(),
                started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                model_context_window: ctx.model_context_window(),
                collaboration_mode_kind: ctx.collaboration_mode.mode,
            });
            sess.send_event(ctx.as_ref(), start_event).await;

            let item = TurnItem::EnteredReviewMode(self.entered_review_mode.clone());
            sess.emit_turn_item_started(ctx.as_ref(), &item).await;
            sess.emit_turn_item_completed(ctx.as_ref(), item).await;

            session
                .services
                .session_telemetry
                .counter("codex.task.review", /*inc*/ 1, &[]);

            let mut user_input = Vec::new();
            for item in input {
                match item {
                    TurnInput::UserInput { mut content, .. } => user_input.append(&mut content),
                    TurnInput::ResponseItem(_)
                    | TurnInput::InternalResponseItem(_)
                    | TurnInput::InterAgentCommunication(_) => {}
                }
            }

            // Start sub-codex conversation and get the receiver for events.
            let standalone_work_guard = ctx.turn_timing_state.begin_standalone_work();
            let output = match start_review_conversation(
                session.clone(),
                ctx.clone(),
                user_input,
                cancellation_token.clone(),
            )
            .await
            {
                Ok(receiver) => process_review_events(session.clone(), ctx.clone(), receiver).await,
                Err(err) => {
                    if !cancellation_token.is_cancelled() {
                        let item = TurnItem::ExitedReviewMode(ExitedReviewModeItem {
                            id: uuid::Uuid::now_v7().to_string(),
                            review_output: None,
                        });
                        session.emit_turn_item_started(ctx.as_ref(), &item).await;
                        session.emit_turn_item_completed(ctx.as_ref(), item).await;
                    }
                    return Err(err);
                }
            };
            drop(standalone_work_guard);
            if !cancellation_token.is_cancelled() {
                exit_review_mode(Arc::clone(&session), output.clone(), ctx.clone()).await;
            }
            Ok(super::TurnTaskResult::default())
        })
    }

    fn abort<'a>(&'a self, session: Arc<Session>, ctx: Arc<TurnContext>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            exit_review_mode(session, /*review_output*/ None, ctx).await;
        })
    }
}

async fn start_review_conversation(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> codex_protocol::error::Result<async_channel::Receiver<Event>> {
    let config = ctx.config.clone();
    let mut sub_agent_config = config.as_ref().clone();
    // Carry over review-only feature restrictions so the delegate cannot
    // re-enable blocked tools (web search, collab tools, view image).
    if let Err(err) = sub_agent_config
        .web_search_mode
        .set(WebSearchMode::Disabled)
    {
        panic!("by construction Constrained<WebSearchMode> must always support Disabled: {err}");
    }
    let _ = sub_agent_config.features.disable(Feature::SpawnCsv);
    let _ = sub_agent_config.features.disable(Feature::Collab);
    let _ = sub_agent_config.features.disable(Feature::MultiAgentV2);

    // Set explicit review rubric for the sub-agent
    sub_agent_config.base_instructions = Some(crate::REVIEW_PROMPT.to_string());
    sub_agent_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);

    let model = config
        .review_model
        .clone()
        .unwrap_or_else(|| ctx.model_info.slug.clone());
    sub_agent_config.model = Some(model);
    run_codex_thread_one_shot(
        sub_agent_config,
        Arc::clone(&session.services.auth_manager),
        Arc::clone(&session.services.models_manager),
        input,
        Arc::clone(&session),
        ctx.clone(),
        cancellation_token,
        SubAgentSource::Review,
        /*final_output_json_schema*/ None,
        /*initial_history*/ None,
    )
    .await
    .map(|io| io.rx_event)
}

async fn process_review_events(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    receiver: async_channel::Receiver<Event>,
) -> Option<ReviewOutputEvent> {
    let mut prev_agent_message: Option<Event> = None;
    while let Ok(event) = receiver.recv().await {
        match event.clone().msg {
            EventMsg::AgentMessage(_) => {
                if let Some(prev) = prev_agent_message.take() {
                    session.send_event(ctx.as_ref(), prev.msg).await;
                }
                prev_agent_message = Some(event);
            }
            // Suppress ItemCompleted only for assistant messages: forwarding it
            // would trigger legacy AgentMessage via as_legacy_events(), which this
            // review flow intentionally hides in favor of structured output.
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(_),
                ..
            })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            // The parent review task owns the visible turn lifecycle. Forwarding the
            // delegate's start would expose two `TurnStarted` events for one review turn.
            EventMsg::TurnStarted(_) => {}
            EventMsg::TurnComplete(task_complete) => {
                if let Some(error) = task_complete.error {
                    session
                        .send_event(ctx.as_ref(), EventMsg::Error(error))
                        .await;
                    return None;
                }
                // Parse review output from the last agent message (if present).
                let out = task_complete
                    .last_agent_message
                    .as_deref()
                    .and_then(parse_review_output_event);
                if out.is_none() {
                    session
                        .send_event(
                            ctx.as_ref(),
                            EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                                message: "Review did not return a valid structured result. Re-run /review; this is not a clean review verdict.".to_string(),
                                codex_error_info: None,
                            }),
                        )
                        .await;
                }
                return out;
            }
            EventMsg::TurnAborted(_) => {
                // Cancellation or abort: consumer will finalize with None.
                return None;
            }
            other => {
                session.send_event(ctx.as_ref(), other).await;
            }
        }
    }
    // Channel closed without TurnComplete: treat as interrupted.
    None
}

/// Parse a ReviewOutputEvent from a text blob returned by the reviewer model.
/// Accept only results satisfying the review contract. Keep the wire types
/// compatible with saved reviews; validate new model output at this boundary.
/// A streaming deserializer finds an object's actual end even when surrounding
/// prose or code contains braces. Invalid output must not become empty findings.
fn parse_review_output_event(text: &str) -> Option<ReviewOutputEvent> {
    for (start, _) in text.match_indices('{') {
        let Some(Ok(output)) = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<ReviewOutputEvent>()
            .next()
        else {
            continue;
        };
        if matches!(
            output.overall_correctness.as_str(),
            "patch is correct" | "patch is incorrect"
        ) && (0.0..=1.0).contains(&output.overall_confidence_score)
            && output.findings.iter().all(|finding| {
                (0.0..=1.0).contains(&finding.confidence_score)
                    && (0..=3).contains(&finding.priority)
                    && finding.code_location.line_range.start > 0
                    && finding.code_location.line_range.end
                        >= finding.code_location.line_range.start
            })
        {
            return Some(output);
        }
    }
    None
}

/// Emits ExitedReviewMode item lifecycle with optional ReviewOutput,
/// and records the review output back into conversation history.
pub(crate) async fn exit_review_mode(
    session: Arc<Session>,
    review_output: Option<ReviewOutputEvent>,
    ctx: Arc<TurnContext>,
) {
    let (user_message, assistant_message) = if let Some(out) = review_output.clone() {
        let mut findings_str = String::new();
        let text = out.overall_explanation.trim();
        if !text.is_empty() {
            findings_str.push_str(text);
        }
        if !out.findings.is_empty() {
            let block = format_review_findings_block(&out.findings, /*selection*/ None);
            let _ = write!(findings_str, "\n{block}");
        }
        let rendered = render_review_exit_success(&findings_str);
        let assistant_message = render_review_output_text(&out);
        (rendered, assistant_message)
    } else {
        let rendered = render_review_exit_interrupted();
        let assistant_message =
            "Review was interrupted. Please re-run /review and wait for it to complete."
                .to_string();
        (rendered, assistant_message)
    };

    session
        .record_conversation_items(
            &ctx,
            &[ResponseItem::Message {
                id: Some(ResponseItemId::new("msg")),
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: user_message }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;

    let item = TurnItem::ExitedReviewMode(ExitedReviewModeItem {
        id: uuid::Uuid::now_v7().to_string(),
        review_output,
    });
    session.emit_turn_item_started(ctx.as_ref(), &item).await;
    session.emit_turn_item_completed(ctx.as_ref(), item).await;
    session
        .record_response_item_and_emit_turn_item(
            ctx.as_ref(),
            ResponseItem::Message {
                id: Some(ResponseItemId::new("msg")),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: assistant_message,
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        )
        .await;

    // Review turns can run before any regular user turn, so explicitly
    // materialize rollout persistence. Do this after emitting review output so
    // file creation + git metadata collection cannot delay client-facing items.
    session.ensure_rollout_materialized().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::TurnCompleteEvent;

    fn valid_review() -> serde_json::Value {
        serde_json::json!({
            "findings": [{
                "title": "[P2] Preserve the result",
                "body": "This change discards the result.",
                "confidence_score": 0.8,
                "priority": 2,
                "code_location": {
                    "absolute_file_path": "/repo/file.rs",
                    "line_range": {"start": 1, "end": 2}
                }
            }],
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "The result is discarded.",
            "overall_confidence_score": 0.9
        })
    }

    #[test]
    fn review_parser_recovers_json_among_unrelated_braces() {
        let value = valid_review();
        let expected = serde_json::from_value::<ReviewOutputEvent>(value.clone()).unwrap();
        for text in [
            value.to_string(),
            format!("Example: fn main() {{}}\n```json\n{value}\n```\nTrailing {{ prose }}"),
        ] {
            assert_eq!(parse_review_output_event(&text), Some(expected.clone()));
        }
    }

    #[test]
    fn review_parser_rejects_invalid_contract_fields() {
        for (pointer, replacement) in [
            ("/overall_correctness", serde_json::json!("good")),
            ("/overall_confidence_score", serde_json::json!(1.1)),
            ("/overall_confidence_score", serde_json::json!(-0.1)),
            ("/findings/0/confidence_score", serde_json::json!(1.1)),
            ("/findings/0/confidence_score", serde_json::json!(-0.1)),
            ("/findings/0/priority", serde_json::json!(4)),
            ("/findings/0/priority", serde_json::json!(-1)),
            ("/findings/0/priority", serde_json::Value::Null),
            (
                "/findings/0/code_location/line_range/start",
                serde_json::json!(0),
            ),
            (
                "/findings/0/code_location/line_range/end",
                serde_json::json!(0),
            ),
        ] {
            let mut value = valid_review();
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert_eq!(
                parse_review_output_event(&value.to_string()),
                None,
                "{pointer}"
            );
        }
        assert_eq!(parse_review_output_event("plain text"), None);
        let mut value = valid_review();
        value["findings"][0]
            .as_object_mut()
            .unwrap()
            .remove("priority");
        assert_eq!(parse_review_output_event(&value.to_string()), None);
    }

    #[test]
    fn review_parser_accepts_clean_verdict_and_numeric_boundaries() {
        let mut value = valid_review();
        for priority in 0..=3 {
            for confidence in [0.0, 1.0] {
                value["findings"][0]["priority"] = serde_json::json!(priority);
                value["findings"][0]["confidence_score"] = serde_json::json!(confidence);
                value["overall_confidence_score"] = serde_json::json!(confidence);
                assert!(parse_review_output_event(&value.to_string()).is_some());
            }
        }
        value["findings"] = serde_json::json!([]);
        value["overall_correctness"] = serde_json::json!("patch is correct");
        assert!(parse_review_output_event(&value.to_string()).is_some());
    }

    #[tokio::test]
    async fn failed_delegate_completion_preserves_error_and_closes_review_mode() {
        let (session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (sender, receiver) = async_channel::unbounded();
        let error = ErrorEvent {
            message: "review delegate failed before completing".to_string(),
            codex_error_info: Some(CodexErrorInfo::InternalServerError),
        };
        sender
            .send(Event {
                id: "delegate-turn".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "delegate-turn".to_string(),
                    last_agent_message: Some(
                        "partial review must not count as success".to_string(),
                    ),
                    surfaced_result: None,
                    error: Some(error.clone()),
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                }),
            })
            .await
            .expect("delegate event receiver is open");
        drop(sender);

        let output = process_review_events(Arc::clone(&session), Arc::clone(&ctx), receiver).await;
        assert!(
            output.is_none(),
            "failed delegate output must not become a successful review"
        );
        assert_eq!(*ctx.terminal_error.lock().await, Some(error.clone()));
        exit_review_mode(Arc::clone(&session), output, Arc::clone(&ctx)).await;

        let mut errors = Vec::new();
        let mut exits = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::Error(error) => errors.push(error),
                EventMsg::ItemCompleted(ItemCompletedEvent {
                    item: TurnItem::ExitedReviewMode(exited),
                    ..
                }) => exits.push(exited.review_output),
                _ => {}
            }
        }
        assert_eq!(errors, vec![error]);
        assert_eq!(exits, vec![None]);
    }
}
