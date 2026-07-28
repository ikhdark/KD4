use std::sync::Arc;

use codex_prompts::render_review_exit_interrupted;
use codex_prompts::render_review_exit_success;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::ExitedReviewModeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
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
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::run_codex_thread_one_shot;
use crate::config::Constrained;
use crate::config::Permissions;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::task_evidence::TaskEvidenceReviewPacket;
use crate::task_evidence::TaskLifecycleStatus;
use crate::task_evidence::TaskReviewReceipt;
use codex_features::Feature;
use codex_protocol::user_input::UserInput;

use super::SessionTask;
use super::SessionTaskContext;
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

pub(crate) async fn run_task_evidence_review(
    session: Arc<Session>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    ctx: Arc<TurnContext>,
    packet: TaskEvidenceReviewPacket,
    cancellation_token: CancellationToken,
) -> Result<Option<TaskLifecycleStatus>, String> {
    let task_session = Arc::new(SessionTaskContext::new(
        Arc::clone(&session),
        turn_extension_data,
    ));
    let input = vec![UserInput::Text {
        text: packet.prompt,
        text_elements: Vec::new(),
    }];
    let receiver = match start_review_conversation(
        Arc::clone(&task_session),
        Arc::clone(&ctx),
        input,
        cancellation_token.clone(),
    )
    .await
    {
        Ok(receiver) => receiver,
        Err(_) if cancellation_token.is_cancelled() => return Ok(None),
        Err(err) => return Err(err),
    };
    let (output, structured) =
        process_review_events(task_session, Arc::clone(&ctx), receiver).await;
    if cancellation_token.is_cancelled() {
        return Ok(None);
    }
    let receipt = task_review_receipt(output.as_ref(), structured);
    let status = session
        .services
        .task_evidence
        .accept_review(&packet.binding_hash, receipt)
        .await?;
    let reviewer_report = output
        .as_ref()
        .map(render_review_output_text)
        .unwrap_or_else(|| "Independent review produced no structured output.".to_string());
    let structure_note = if structured {
        ""
    } else {
        "\nThe reviewer output was not exact structured JSON and could not authorize a clean verdict."
    };
    session
        .record_conversation_items(
            &ctx,
            &[ResponseItem::Message {
                id: Some(ResponseItemId::new("msg")),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!(
                        "Runtime independent review: {}\n\n{}{}",
                        status.message, reviewer_report, structure_note
                    ),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    Ok(Some(status))
}

fn task_review_receipt(output: Option<&ReviewOutputEvent>, structured: bool) -> TaskReviewReceipt {
    let findings = output
        .filter(|_| structured)
        .map(|output| {
            output
                .findings
                .iter()
                .filter_map(|finding| serde_json::to_string(finding).ok())
                .collect()
        })
        .unwrap_or_default();
    let confidence_score_millis = output
        .filter(|_| structured)
        .map(|output| output.overall_confidence_score)
        .filter(|confidence| confidence.is_finite() && (0.0..=1.0).contains(confidence))
        .map(|confidence| (confidence * 1000.0).round() as u16)
        .unwrap_or(1001);
    TaskReviewReceipt {
        findings,
        verdict: output
            .filter(|_| structured)
            .map(|output| output.overall_correctness.clone())
            .unwrap_or_default(),
        explanation: output
            .filter(|_| structured)
            .map(|output| output.overall_explanation.clone())
            .unwrap_or_else(|| "independent review was interrupted".to_string()),
        confidence_score_millis,
    }
}

impl SessionTask for ReviewTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Review
    }

    fn span_name(&self) -> &'static str {
        "session_task.review"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let sess = session.clone_session();
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

        session.session.services.session_telemetry.counter(
            "codex.task.review",
            /*inc*/ 1,
            &[],
        );

        let mut user_input = Vec::new();
        for item in input {
            match item {
                TurnInput::UserInput { mut content, .. } => user_input.append(&mut content),
                TurnInput::ResponseItem(_) | TurnInput::InterAgentCommunication(_) => {}
            }
        }

        // Start sub-codex conversation and get the receiver for events.
        let standalone_work_guard = ctx.turn_timing_state.begin_standalone_work();
        let (output, _structured) = match start_review_conversation(
            session.clone(),
            ctx.clone(),
            user_input,
            cancellation_token.clone(),
        )
        .await
        {
            Ok(receiver) => process_review_events(session.clone(), ctx.clone(), receiver).await,
            Err(_) => (None, false),
        };
        drop(standalone_work_guard);
        if !cancellation_token.is_cancelled() {
            exit_review_mode(session.clone_session(), output.clone(), ctx.clone()).await;
        }
        Ok(None)
    }

    async fn abort(&self, session: Arc<SessionTaskContext>, ctx: Arc<TurnContext>) {
        exit_review_mode(session.clone_session(), /*review_output*/ None, ctx).await;
    }
}

async fn start_review_conversation(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Result<async_channel::Receiver<Event>, String> {
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
    sub_agent_config.permissions = Permissions::from_approval_and_profile(
        Constrained::allow_only(AskForApproval::Never),
        Constrained::allow_only(PermissionProfile::read_only()),
    )
    .expect("the built-in read-only reviewer profile is valid");
    sub_agent_config.notify = None;
    sub_agent_config.bypass_hook_trust = false;
    sub_agent_config.ephemeral = true;

    let model = config
        .review_model
        .clone()
        .unwrap_or_else(|| ctx.model_info.slug.clone());
    sub_agent_config.model = Some(model);
    run_codex_thread_one_shot(
        sub_agent_config,
        session.auth_manager(),
        session.models_manager(),
        input,
        session.clone_session(),
        ctx.clone(),
        cancellation_token,
        SubAgentSource::Review,
        /*final_output_json_schema*/ None,
        /*initial_history*/ None,
    )
    .await
    .map(|io| io.rx_event)
    .map_err(|err| format!("failed to start isolated independent reviewer: {err:#}"))
}

async fn process_review_events(
    session: Arc<SessionTaskContext>,
    ctx: Arc<TurnContext>,
    receiver: async_channel::Receiver<Event>,
) -> (Option<ReviewOutputEvent>, bool) {
    let mut prev_agent_message: Option<Event> = None;
    while let Ok(event) = receiver.recv().await {
        match event.clone().msg {
            EventMsg::AgentMessage(_) => {
                if let Some(prev) = prev_agent_message.take() {
                    session
                        .clone_session()
                        .send_event(ctx.as_ref(), prev.msg)
                        .await;
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
                // Parse review output from the last agent message (if present).
                let out = task_complete
                    .last_agent_message
                    .as_deref()
                    .map(parse_review_output_event);
                return match out {
                    Some((output, structured)) => (Some(output), structured),
                    None => (None, false),
                };
            }
            EventMsg::TurnAborted(_) => {
                // Cancellation or abort: consumer will finalize with None.
                return (None, false);
            }
            other => {
                session
                    .clone_session()
                    .send_event(ctx.as_ref(), other)
                    .await;
            }
        }
    }
    // Channel closed without TurnComplete: treat as interrupted.
    (None, false)
}

/// Parse a ReviewOutputEvent from a text blob returned by the reviewer model.
/// If the text is valid JSON matching ReviewOutputEvent, deserialize it.
/// Otherwise, parse a JSON-looking substring for display only. Surrounding prose
/// or code fences must never authorize a lifecycle review.
fn parse_review_output_event(text: &str) -> (ReviewOutputEvent, bool) {
    if let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(text) {
        return (ev, true);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(slice)
    {
        return (ev, false);
    }
    (
        ReviewOutputEvent {
            overall_explanation: text.to_string(),
            ..Default::default()
        },
        false,
    )
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
            findings_str.push_str(&format!("\n{block}"));
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
