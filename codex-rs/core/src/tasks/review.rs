use std::collections::HashSet;
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

#[derive(serde::Deserialize)]
struct CompletedReviewResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(flatten)]
    output: ReviewOutputEvent,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct BlockedReviewResponse {
    status: String,
    reason: String,
    unreviewed_scope: Vec<String>,
    required_action: String,
}

#[derive(Clone)]
pub(crate) struct ReviewTask {
    entered_review_mode: EnteredReviewModeItem,
    source_scope: Option<codex_git_utils::ReviewSourceScope>,
}

impl ReviewTask {
    pub(crate) fn new(entered_review_mode: EnteredReviewModeItem) -> Self {
        Self {
            entered_review_mode,
            source_scope: None,
        }
    }

    pub(crate) fn with_source_scope(mut self, scope: codex_git_utils::ReviewSourceScope) -> Self {
        self.source_scope = Some(scope);
        self
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
                    | TurnInput::AdditionalContext(_)
                    | TurnInput::InternalResponseItem(_)
                    | TurnInput::InterAgentCommunication(_) => {}
                }
            }

            // Start sub-codex conversation and get the receiver for events.
            let standalone_work_guard = ctx.turn_timing_state.begin_standalone_work();
            let mut output = match start_review_conversation(
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
            if let Some(scope) = self.source_scope.clone()
                && let Some(mut review) = output.take()
            {
                match tokio::task::spawn_blocking(move || {
                    scope.validate(&mut review).map(|()| review)
                })
                .await
                {
                    Ok(Ok(review)) => output = Some(review),
                    result => {
                        let reason = match result {
                            Ok(Err(reason)) => reason,
                            Err(error) => error.to_string(),
                            Ok(Ok(_)) => unreachable!(),
                        };
                        reject_review_source_validation(&session, &ctx, &reason).await;
                    }
                }
            }
            if !cancellation_token.is_cancelled() {
                exit_review_mode(Arc::clone(&session), output, ctx.clone()).await;
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

async fn reject_review_source_validation(
    session: &Session,
    ctx: &TurnContext,
    reason: &str,
) {
    let message = format!("Review source validation failed: {reason}");
    // Error events are UI-only on replay. Retain this host rejection as
    // non-verdict history so a resumed review knows why no result was accepted.
    let item = ResponseItem::Message {
        id: Some(ResponseItemId::new("review-source-rejected")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: format!(
                "Review source validation rejected the result; no completed verdict.\n{message}\nRe-establish the requested source comparison and resolve the rejection before re-running /review."
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    if let Err(error) = session.record_conversation_items(ctx, &[item]).await {
        session
            .send_event(
                ctx,
                EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                    message: format!("Could not retain review source rejection: {error}"),
                    codex_error_info: None,
                }),
            )
            .await;
    }
    session
        .send_event(
            ctx,
            EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                message,
                codex_error_info: None,
            }),
        )
        .await;
}

async fn process_review_events(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    receiver: async_channel::Receiver<Event>,
) -> Option<ReviewOutputEvent> {
    let mut prev_agent_message: Option<codex_protocol::protocol::AgentMessageEvent> = None;
    while let Ok(event) = receiver.recv().await {
        match event.msg {
            EventMsg::AgentMessage(message) => {
                if let Some(message) = prev_agent_message.take() {
                    let item = TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                        id: uuid::Uuid::now_v7().to_string(),
                        content: vec![codex_protocol::items::AgentMessageContent::Text {
                            text: message.message,
                        }],
                        phase: message.phase,
                    });
                    session.emit_turn_item_started(&ctx, &item).await;
                    session.emit_turn_item_completed(&ctx, item).await;
                }
                prev_agent_message = Some(message);
            }
            // Suppress the entire hidden assistant lifecycle: forwarding it
            // would trigger legacy AgentMessage via as_legacy_events(), which this
            // review flow intentionally hides in favor of structured output.
            EventMsg::ItemStarted(codex_protocol::protocol::ItemStartedEvent {
                item: TurnItem::AgentMessage(_),
                ..
            })
            | EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(_),
                ..
            })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            // The parent review task owns the visible turn lifecycle. Forwarding the
            // delegate's start would expose two `TurnStarted` events for one review turn.
            EventMsg::TurnStarted(_) => {}
            // The reviewer's OwnTask ledger is not a parent plan update. In
            // particular, persisting it here would replace parent scope on resume.
            EventMsg::PlanUpdate(_) => {}
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
                    // Keep blocked outcomes machine-readable without treating them as
                    // successful reviews or changing the persisted completed wire type.
                    let blocked = task_complete
                        .last_agent_message
                        .as_deref()
                        .and_then(parse_blocked_review_output)
                        .and_then(|blocked| serde_json::to_string(&blocked).ok());
                    if let Some(blocked) = &blocked {
                        // Error events reach the UI but are not replayed as model
                        // input. Retain the exact recovery scope for follow-up and
                        // resume without inventing a completed review verdict.
                        let item = ResponseItem::Message {
                            id: Some(ResponseItemId::new("review-blocked")),
                            role: "assistant".to_string(),
                            content: vec![ContentItem::OutputText {
                                text: format!("Review blocked; no completed verdict.\n{blocked}"),
                            }],
                            phase: None,
                            internal_chat_message_metadata_passthrough: None,
                        };
                        if let Err(error) = session.record_conversation_items(&ctx, &[item]).await {
                            session
                                .send_event(
                                    ctx.as_ref(),
                                    EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                                        message: format!(
                                            "Could not retain blocked-review recovery context: {error}"
                                        ),
                                        codex_error_info: None,
                                    }),
                                )
                                .await;
                        }
                    }
                    let message = blocked.unwrap_or_else(|| {
                        "Review did not return a valid structured result. Re-run /review; this is not a clean review verdict.".to_string()
                    });
                    session
                        .send_event(
                            ctx.as_ref(),
                            EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                                message,
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
            mut other => {
                reparent_review_event(&mut other, session.thread_id, &ctx.sub_id);
                session.send_event(ctx.as_ref(), other).await;
            }
        }
    }
    // Channel closed without TurnComplete: treat as interrupted.
    None
}

// The parent owns the only announced lifecycle. Namespace provider IDs and
// normalize every surfaced payload owner to that parent lifecycle.
fn reparent_review_event(event: &mut EventMsg, thread_id: codex_protocol::ThreadId, turn_id: &str) {
    match event {
        EventMsg::ItemStarted(event) => {
            event.thread_id = thread_id;
            event.turn_id = turn_id.to_string();
            namespace_review_item(&mut event.item, turn_id);
        }
        EventMsg::ItemCompleted(event) => {
            event.thread_id = thread_id;
            event.turn_id = turn_id.to_string();
            namespace_review_item(&mut event.item, turn_id);
        }
        EventMsg::ReasoningContentDelta(event) => {
            event.thread_id = thread_id.to_string();
            event.turn_id = turn_id.to_string();
            event.item_id = format!("review-{turn_id}:{}", event.item_id);
        }
        EventMsg::ReasoningRawContentDelta(event) => {
            event.thread_id = thread_id.to_string();
            event.turn_id = turn_id.to_string();
            event.item_id = format!("review-{turn_id}:{}", event.item_id);
        }
        EventMsg::PlanDelta(event) => {
            event.thread_id = thread_id.to_string();
            event.turn_id = turn_id.to_string();
            event.item_id = format!("review-{turn_id}:{}", event.item_id);
        }
        EventMsg::AgentReasoningSectionBreak(event) => {
            event.item_id = format!("review-{turn_id}:{}", event.item_id)
        }
        EventMsg::ExecCommandBegin(event) => {
            event.turn_id = turn_id.to_string();
            event.call_id = format!("review-{turn_id}:{}", event.call_id);
        }
        EventMsg::ExecCommandEnd(event) => {
            event.turn_id = turn_id.to_string();
            event.call_id = format!("review-{turn_id}:{}", event.call_id);
        }
        EventMsg::ExecCommandOutputDelta(event) => {
            event.call_id = format!("review-{turn_id}:{}", event.call_id)
        }
        EventMsg::TerminalInteraction(event) => {
            event.call_id = format!("review-{turn_id}:{}", event.call_id)
        }
        EventMsg::McpToolCallBegin(event) => {
            event.call_id = format!("review-{turn_id}:{}", event.call_id)
        }
        EventMsg::McpToolCallEnd(event) => {
            event.call_id = format!("review-{turn_id}:{}", event.call_id)
        }
        EventMsg::DynamicToolCallResponse(event) => {
            event.turn_id = turn_id.to_string();
            event.call_id = format!("review-{turn_id}:{}", event.call_id);
        }
        EventMsg::PatchApplyBegin(event) => event.turn_id = turn_id.to_string(),
        EventMsg::PatchApplyEnd(event) => event.turn_id = turn_id.to_string(),
        _ => {}
    }
}

fn namespace_review_item(item: &mut TurnItem, turn_id: &str) {
    macro_rules! namespace {
        ($item:ident) => {
            $item.id = format!("review-{turn_id}:{}", $item.id)
        };
    }
    match item {
        TurnItem::UserMessage(item) => namespace!(item),
        TurnItem::HookPrompt(item) => namespace!(item),
        TurnItem::AgentMessage(item) => namespace!(item),
        TurnItem::Plan(item) => namespace!(item),
        TurnItem::Reasoning(item) => namespace!(item),
        TurnItem::CommandExecution(item) => {
            namespace!(item);
            if let Some(parent) = &mut item.parent_call_id {
                *parent = format!("review-{turn_id}:{parent}");
            }
        }
        TurnItem::DynamicToolCall(item) => namespace!(item),
        TurnItem::CollabAgentToolCall(item) => namespace!(item),
        TurnItem::SubAgentActivity(item) => namespace!(item),
        TurnItem::WebSearch(item) => namespace!(item),
        TurnItem::ImageView(item) => namespace!(item),
        TurnItem::Sleep(item) => namespace!(item),
        TurnItem::ImageGeneration(item) => namespace!(item),
        TurnItem::EnteredReviewMode(item) => namespace!(item),
        TurnItem::ExitedReviewMode(item) => namespace!(item),
        TurnItem::FileChange(item) => namespace!(item),
        TurnItem::McpToolCall(item) => namespace!(item),
        TurnItem::ContextCompaction(item) => namespace!(item),
        // Extension-owned IDs are already host-generated, not provider item IDs.
        TurnItem::Extension(_) => {}
    }
}

fn review_output_json(text: &str) -> Option<&str> {
    let text = text.trim();
    if let Some(fenced) = text.strip_prefix("```") {
        let (language, body) = fenced.split_once('\n')?;
        if !matches!(language.trim(), "" | "json") {
            return None;
        }
        Some(body.strip_suffix("```")?.trim())
    } else {
        Some(text)
    }
}

/// Parse a ReviewOutputEvent from a text blob returned by the reviewer model.
/// Accept only results satisfying the review contract. Keep the wire types
/// compatible with saved reviews; validate new model output at this boundary.
/// Accept one complete JSON document, optionally enclosed in one Markdown fence.
/// Examples, wrappers and trailing prose are not authoritative review results.
fn parse_review_output_event(text: &str) -> Option<ReviewOutputEvent> {
    let response: CompletedReviewResponse =
        serde_json::from_str(review_output_json(text)?).ok()?;
    // A tagged blocked/unknown outcome must not fall through to a completed
    // verdict merely because it also contains the legacy completed fields.
    if response.status.is_some() {
        return None;
    }
    let mut output = response.output;
    if !matches!(
        output.overall_correctness.as_str(),
        "patch is correct" | "patch is incorrect"
    ) || !(0.0..=1.0).contains(&output.overall_confidence_score)
        || output.overall_explanation.trim().is_empty()
        || (!output.findings.is_empty() && output.overall_correctness == "patch is correct")
        || !output.findings.iter().all(|finding| {
            (0.0..=1.0).contains(&finding.confidence_score)
                && (0..=3).contains(&finding.priority)
                && !finding.title.trim().is_empty()
                && !finding.body.trim().is_empty()
                && finding.code_location.absolute_file_path.is_absolute()
                && finding.code_location.line_range.start > 0
                && finding.code_location.line_range.end >= finding.code_location.line_range.start
        })
    {
        return None;
    }
    for finding in &mut output.findings {
        finding.title =
            codex_protocol::review_format::canonical_review_title(&finding.title, finding.priority);
        if finding.title.trim().len() <= 4 {
            return None;
        }
    }
    let mut seen = HashSet::new();
    output.findings.retain(|finding| {
        seen.insert((
            finding.title.clone(),
            finding.body.clone(),
            finding.confidence_score.to_bits(),
            finding.priority,
            finding.code_location.absolute_file_path.clone(),
            finding.code_location.line_range.start,
            finding.code_location.line_range.end,
        ))
    });
    Some(output)
}

fn parse_blocked_review_output(text: &str) -> Option<BlockedReviewResponse> {
    let output: BlockedReviewResponse =
        serde_json::from_str(review_output_json(text)?).ok()?;
    (output.status == "blocked"
        && !output.reason.trim().is_empty()
        && !output.unreviewed_scope.is_empty()
        && output
            .unreviewed_scope
            .iter()
            .all(|scope| !scope.trim().is_empty())
        && !output.required_action.trim().is_empty())
    .then_some(output)
}

/// Emits ExitedReviewMode item lifecycle with optional ReviewOutput,
/// and records the review output back into conversation history.
pub(crate) async fn exit_review_mode(
    session: Arc<Session>,
    review_output: Option<ReviewOutputEvent>,
    ctx: Arc<TurnContext>,
) {
    let (user_message, assistant_message) = if let Some(out) = review_output.as_ref() {
        let rendered = render_review_exit_success(
            "The review explanation and findings are in the following assistant message.",
        );
        let assistant_message = render_review_output_text(out);
        (rendered, assistant_message)
    } else {
        let rendered = render_review_exit_interrupted();
        let assistant_message =
            "The review produced no result (blocked, interrupted, failed, or invalid output). Resolve any reported blocker before re-running /review."
                .to_string();
        (rendered, assistant_message)
    };

    if let Err(error) = session
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
        .await
    {
        session
            .send_event(
                &ctx,
                EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                    message: format!("Could not record review output: {error}"),
                    codex_error_info: None,
                }),
            )
            .await;
        return;
    }

    let exit_id = format!("review-exit:{}", uuid::Uuid::now_v7());
    let mirror_id = ResponseItemId::with_suffix("review-result", &exit_id);
    let item = TurnItem::ExitedReviewMode(ExitedReviewModeItem {
        id: exit_id,
        review_output,
    });
    session.emit_turn_item_started(ctx.as_ref(), &item).await;
    session.emit_turn_item_completed(ctx.as_ref(), item).await;
    if let Err(error) = session
        .record_response_item_and_emit_turn_item(
            ctx.as_ref(),
            ResponseItem::Message {
                id: Some(mirror_id),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: assistant_message,
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        )
        .await
    {
        session
            .send_event(
                &ctx,
                EventMsg::Error(codex_protocol::protocol::ErrorEvent {
                    message: format!("Could not record review assistant output: {error}"),
                    codex_error_info: None,
                }),
            )
            .await;
    }

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
                    "absolute_file_path": std::env::temp_dir().join("review-file.rs"),
                    "line_range": {"start": 1, "end": 2}
                }
            }],
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "The result is discarded.",
            "overall_confidence_score": 0.9
        })
    }

    #[test]
    fn review_parser_accepts_one_document_or_fence_with_string_braces() {
        let mut value = valid_review();
        value["overall_explanation"] =
            serde_json::json!("The {result} contains braces and ``` text.");
        let expected = serde_json::from_value::<ReviewOutputEvent>(value.clone()).unwrap();
        for text in [
            value.to_string(),
            format!(" \n```json\n{value}\n```\n"),
            format!("```\r\n{value}\r\n```"),
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

    #[test]
    fn review_parser_rejects_competing_results_in_either_order_or_wrapper() {
        let defect = valid_review();
        let mut clean = defect.clone();
        clean["findings"] = serde_json::json!([]);
        clean["overall_correctness"] = serde_json::json!("patch is correct");
        for text in [
            format!("{clean}\n{defect}"),
            format!("{defect}\n{clean}"),
            format!("Example: {clean}\nActual result: {defect}"),
            format!("{clean}\n{{}}"),
            format!("```json\n{clean}\n``` trailing prose"),
            format!("Example: {clean}"),
            serde_json::json!({"example": clean}).to_string(),
            serde_json::json!([clean]).to_string(),
            serde_json::json!({"example": clean, "actual": defect}).to_string(),
        ] {
            assert!(parse_review_output_event(&text).is_none());
        }
    }

    #[test]
    fn review_parser_requires_substance_and_verdict_coherence() {
        for (pointer, replacement) in [
            ("/overall_correctness", "patch is correct"),
            ("/overall_explanation", " \n\t"),
            ("/findings/0/title", "[P2]"),
            ("/findings/0/body", "\u{2003}"),
            (
                "/findings/0/code_location/absolute_file_path",
                "relative.rs",
            ),
        ] {
            let mut value = valid_review();
            *value.pointer_mut(pointer).unwrap() = serde_json::json!(replacement);
            assert!(
                parse_review_output_event(&value.to_string()).is_none(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn review_parser_deduplicates_only_exact_normalized_findings() {
        let mut value = valid_review();
        let first = value["findings"][0].clone();
        let mut second = first.clone();
        second["body"] = serde_json::json!("A distinct violated invariant at the same location.");
        value["findings"] = serde_json::json!([first, second, first]);
        let output = parse_review_output_event(&value.to_string()).unwrap();
        assert_eq!(output.findings.len(), 2);
        assert_eq!(
            output.findings[1].body,
            "A distinct violated invariant at the same location."
        );
        value["findings"][0]["title"] = serde_json::json!("[P0] Preserve the result");
        assert_eq!(
            parse_review_output_event(&value.to_string())
                .unwrap()
                .findings[0]
                .title,
            "[P2] Preserve the result"
        );
    }

    #[tokio::test]
    async fn ambiguous_review_never_publishes_success_or_clean_history() {
        let defect = valid_review();
        let mut clean = defect.clone();
        clean["findings"] = serde_json::json!([]);
        clean["overall_correctness"] = serde_json::json!("patch is correct");
        let mut contradictory = defect.clone();
        contradictory["overall_correctness"] = serde_json::json!("patch is correct");
        for text in [
            format!("Example: {clean}\nActual result: {defect}"),
            serde_json::json!({"example":clean}).to_string(),
            contradictory.to_string(),
        ] {
            let (mut session, ctx, events) =
                crate::session::tests::make_session_and_context_with_rx().await;
            let path = crate::session::tests::attach_thread_persistence(
                Arc::get_mut(&mut session).unwrap(),
            )
            .await;
            let (sender, receiver) = async_channel::unbounded();
            sender
                .send(Event {
                    id: "review".into(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "review".into(),
                        last_agent_message: Some(text),
                        surfaced_result: None,
                        error: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                        timing: None,
                    }),
                })
                .await
                .unwrap();
            drop(sender);
            let output =
                process_review_events(Arc::clone(&session), Arc::clone(&ctx), receiver).await;
            assert!(output.is_none());
            exit_review_mode(Arc::clone(&session), output, Arc::clone(&ctx)).await;
            let exits = std::iter::from_fn(|| events.try_recv().ok())
                .filter_map(|event| {
                    if let EventMsg::ItemCompleted(ItemCompletedEvent {
                        item: TurnItem::ExitedReviewMode(exited),
                        ..
                    }) = event.msg
                    {
                        Some(exited.review_output)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(exits, vec![None]);
            session.live_thread().unwrap().shutdown().await.unwrap();
            let (items, _, errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
                .await
                .unwrap();
            assert_eq!(errors, 0);
            let assistant = items
                .iter()
                .filter_map(|item| {
                    if let codex_protocol::protocol::RolloutItem::ResponseItem(
                        ResponseItem::Message { role, content, .. },
                    ) = item
                        && role == "assistant"
                    {
                        Some(content)
                    } else {
                        None
                    }
                })
                .flatten()
                .filter_map(|item| {
                    if let ContentItem::OutputText { text } = item {
                        Some(text)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert!(assistant.iter().any(|text| text.contains("no result")));
            assert!(
                assistant
                    .iter()
                    .all(|text| !text.contains("patch is correct"))
            );
        }
    }

    #[tokio::test]
    async fn blocked_review_preserves_recovery_scope_in_history_and_resume() {
        let blocked = serde_json::json!({
            "status": "blocked",
            "reason": "A required source directory is unavailable.",
            "unreviewed_scope": ["src/missing.rs:1-40", "its request consumer"],
            "required_action": "Restore the source directory, then finish the unread scope."
        });
        let (mut session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let path = crate::session::tests::attach_thread_persistence(
            Arc::get_mut(&mut session).unwrap(),
        )
        .await;
        let (sender, receiver) = async_channel::unbounded();
        sender
            .send(Event {
                id: "review".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "review".into(),
                    last_agent_message: Some(blocked.to_string()),
                    surfaced_result: None,
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                }),
            })
            .await
            .unwrap();
        drop(sender);

        let output = process_review_events(Arc::clone(&session), Arc::clone(&ctx), receiver).await;
        assert!(output.is_none());
        exit_review_mode(Arc::clone(&session), output, Arc::clone(&ctx)).await;
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(emitted.iter().any(|event| {
            matches!(&event.msg, EventMsg::Error(error)
                if serde_json::from_str::<serde_json::Value>(&error.message).ok().as_ref()
                    == Some(&blocked))
        }));
        assert!(emitted.iter().any(|event| {
            matches!(&event.msg, EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::ExitedReviewMode(exited), ..
            }) if exited.review_output.is_none())
        }));
        let assert_recovery = |items: &[ResponseItem]| {
            assert!(
                items.iter().any(|item| {
                    matches!(item, ResponseItem::Message { role, content, .. }
                        if role == "assistant" && content.iter().any(|part| {
                            matches!(part, ContentItem::OutputText { text }
                                if text.strip_prefix("Review blocked; no completed verdict.\n")
                                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                                    .as_ref() == Some(&blocked))
                        }))
                }),
                "blocked scope and recovery action must remain model-visible"
            );
        };
        assert_recovery(session.clone_history().await.raw_items());
        session.live_thread().unwrap().shutdown().await.unwrap();
        let (items, _, errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap();
        assert_eq!(errors, 0);
        assert_recovery(&Session::reconstruct_model_history_from_rollout(&items));
    }

    #[tokio::test]
    async fn source_validation_rejection_preserves_reason_in_history_and_resume() {
        let (mut session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let path = crate::session::tests::attach_thread_persistence(
            Arc::get_mut(&mut session).unwrap(),
        )
        .await;
        let reason = "review source changed after admission; no current-source verdict was published";
        reject_review_source_validation(&session, &ctx, reason).await;
        exit_review_mode(Arc::clone(&session), None, Arc::clone(&ctx)).await;

        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(emitted.iter().any(|event| {
            matches!(&event.msg, EventMsg::Error(error)
                if error.message == format!("Review source validation failed: {reason}"))
        }));
        assert!(emitted.iter().any(|event| {
            matches!(&event.msg, EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::ExitedReviewMode(exited), ..
            }) if exited.review_output.is_none())
        }));
        let assert_recovery = |items: &[ResponseItem]| {
            assert!(items.iter().any(|item| {
                matches!(item, ResponseItem::Message { role, content, .. }
                    if role == "assistant" && content.iter().any(|part| {
                        matches!(part, ContentItem::OutputText { text }
                            if text.starts_with("Review source validation rejected the result; no completed verdict.\n")
                                && text.contains(reason)
                                && text.contains("Re-establish the requested source comparison"))
                    }))
            }));
        };
        assert_recovery(session.clone_history().await.raw_items());
        session.live_thread().unwrap().shutdown().await.unwrap();
        let (items, _, errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap();
        assert_eq!(errors, 0);
        assert_recovery(&Session::reconstruct_model_history_from_rollout(&items));
    }

    #[tokio::test]
    async fn audit32_review_bridge_does_not_publish_child_plans_as_parent_state() {
        use codex_protocol::plan_tool::PlanItemArg;
        use codex_protocol::plan_tool::StepStatus;
        use codex_protocol::plan_tool::UpdatePlanArgs;
        for parent_has_plan in [false, true] {
            let (mut session, ctx, events) =
                crate::session::tests::make_session_and_context_with_rx().await;
            let path = crate::session::tests::attach_thread_persistence(
                Arc::get_mut(&mut session).unwrap(),
            )
            .await;
            let parent = UpdatePlanArgs {
                explanation: None,
                plan: vec![PlanItemArg {
                    step: "Implement the parent request".into(),
                    status: StepStatus::Pending,
                }],
            };
            if parent_has_plan {
                session.services.plan_store.restore(Some(parent)).await;
            }
            let before = session.services.plan_store.snapshot_with_lineage().await;
            let revision = session.services.plan_store.execution_snapshot().await;
            let (sender, receiver) = async_channel::unbounded();
            for status in [StepStatus::Pending, StepStatus::Completed] {
                sender
                    .send(Event {
                        id: "child-review".into(),
                        msg: EventMsg::PlanUpdate(UpdatePlanArgs {
                            explanation: None,
                            plan: vec![PlanItemArg {
                                step: "Independent review work".into(),
                                status,
                            }],
                        }),
                    })
                    .await
                    .unwrap();
            }
            sender
                .send(Event {
                    id: "child-review".into(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "child-review".into(),
                        last_agent_message: Some(valid_review().to_string()),
                        surfaced_result: None,
                        error: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                        timing: None,
                    }),
                })
                .await
                .unwrap();
            drop(sender);
            assert!(
                process_review_events(Arc::clone(&session), ctx, receiver)
                    .await
                    .is_some()
            );
            assert_eq!(
                session.services.plan_store.snapshot_with_lineage().await,
                before
            );
            assert_eq!(
                session.services.plan_store.execution_snapshot().await,
                revision
            );
            assert!(
                !std::iter::from_fn(|| events.try_recv().ok())
                    .any(|event| matches!(event.msg, EventMsg::PlanUpdate(_)))
            );
            session.live_thread().unwrap().shutdown().await.unwrap();
            let (items, _, errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
                .await
                .unwrap();
            assert_eq!(errors, 0);
            assert!(!items.iter().any(|item| matches!(
                item,
                codex_protocol::protocol::RolloutItem::EventMsg(EventMsg::PlanUpdate(_))
            )));
        }
    }

    #[tokio::test]
    async fn review_bridge_hides_assistant_starts_and_reparents_evidence_pairs() {
        let (session, ctx, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (sender, receiver) = async_channel::unbounded();
        let child = codex_protocol::ThreadId::new();
        for item in [
            TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                id: "hidden".into(),
                content: Vec::new(),
                phase: None,
            }),
            TurnItem::Reasoning(codex_protocol::items::ReasoningItem {
                id: "reasoning".into(),
                summary_text: vec!["Inspecting the caller.".into()],
                raw_content: Vec::new(),
            }),
        ] {
            sender
                .send(Event {
                    id: "child-turn".into(),
                    msg: EventMsg::ItemStarted(codex_protocol::protocol::ItemStartedEvent {
                        thread_id: child,
                        turn_id: "child-turn".into(),
                        item: item.clone(),
                        started_at_ms: 0,
                    }),
                })
                .await
                .unwrap();
            sender
                .send(Event {
                    id: "child-turn".into(),
                    msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: child,
                        turn_id: "child-turn".into(),
                        item,
                        completed_at_ms: 0,
                    }),
                })
                .await
                .unwrap();
        }
        drop(sender);
        assert!(
            process_review_events(Arc::clone(&session), Arc::clone(&ctx), receiver)
                .await
                .is_none()
        );
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::ItemStarted(event) => {
                    assert_eq!(event.thread_id, session.thread_id);
                    assert_eq!(event.turn_id, ctx.sub_id);
                    starts.push(event.item.id());
                }
                EventMsg::ItemCompleted(event) => {
                    assert_eq!(event.thread_id, session.thread_id);
                    assert_eq!(event.turn_id, ctx.sub_id);
                    ends.push(event.item.id());
                }
                _ => {}
            }
        }
        assert_eq!(starts, vec![format!("review-{}:reasoning", ctx.sub_id)]);
        assert_eq!(starts, ends);
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
