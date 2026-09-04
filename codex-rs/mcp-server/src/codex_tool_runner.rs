//! Asynchronous worker that executes a **Codex** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use crate::exec_approval::handle_exec_approval_request;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::OutgoingNotificationMeta;
use crate::patch_approval::handle_patch_approval_request;
use codex_core::CodexThread;
use codex_core::NewThread;
use codex_core::ThreadManager;
use codex_core::config::Config as CodexConfig;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ElicitationAction;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::SurfacedToolResult;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::RequestId;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tokio_util::sync::CancellationToken;

const TURN_COMPLETE_AFTER_STATUS_ERROR_TIMEOUT: Duration = Duration::from_secs(5);
const FALLBACK_RESPONSE_SEND_TIMEOUT: Duration = Duration::from_millis(100);

struct PendingTurnError {
    error: ErrorEvent,
    deadline: Instant,
}

enum CodexToolEventSource {
    Runtime(Arc<CodexThread>),
    #[cfg(test)]
    Scripted(tokio::sync::mpsc::Receiver<CodexResult<Event>>),
}

impl CodexToolEventSource {
    async fn next_event(&mut self) -> CodexResult<Event> {
        match self {
            Self::Runtime(thread) => thread.next_event().await,
            #[cfg(test)]
            Self::Scripted(receiver) => receiver
                .recv()
                .await
                .expect("scripted MCP event source closed unexpectedly"),
        }
    }

    fn runtime_thread(&self) -> Arc<CodexThread> {
        match self {
            Self::Runtime(thread) => thread.clone(),
            #[cfg(test)]
            Self::Scripted(_) => {
                panic!("scripted MCP event source cannot handle production-only events")
            }
        }
    }
}

/// To adhere to MCP `tools/call` response format, include the Codex
/// `threadId` in the `structured_content` field of the response.
/// Some MCP clients ignore `content` when `structuredContent` is present, so
/// mirror the text there as well.
pub(crate) fn create_call_tool_result_with_thread_id(
    thread_id: ThreadId,
    text: String,
    is_error: Option<bool>,
) -> CallToolResult {
    create_call_tool_result_with_thread_id_and_surfaced_result(thread_id, text, is_error, None)
}

fn create_call_tool_result_with_thread_id_and_surfaced_result(
    thread_id: ThreadId,
    text: String,
    is_error: Option<bool>,
    surfaced_result: Option<SurfacedToolResult>,
) -> CallToolResult {
    let content_text = text;
    let content = vec![Content::text(content_text.clone())];
    let mut structured_content = json!({
        "threadId": thread_id,
        "content": content_text,
    });
    if let Some(surfaced_result) = surfaced_result {
        structured_content["surfacedResult"] = json!(surfaced_result);
    }
    let mut result = CallToolResult::success(content);
    result.is_error = is_error;
    result.structured_content = Some(structured_content);
    result
}

#[derive(Deserialize)]
struct ElicitationCreateResponse {
    action: ElicitationAction,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
}

async fn forward_elicitation(
    event: ElicitationRequestEvent,
    outgoing: Arc<OutgoingMessageSender>,
    thread: Arc<CodexThread>,
    cancellation: CancellationToken,
) {
    if !outgoing.supports_elicitation(&event.request) {
        let _ = thread
            .submit(Op::ResolveElicitation {
                server_name: event.server_name,
                request_id: event.id,
                decision: ElicitationAction::Cancel,
                content: None,
                meta: None,
            })
            .await;
        return;
    }
    let params = event.request.to_mcp_create_params();
    let pending_request = outgoing
        .send_request("elicitation/create", Some(params))
        .await;
    let pending_request_id = pending_request.id;
    let receiver = pending_request.receiver;
    tokio::spawn(async move {
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                outgoing.cancel_request(&pending_request_id).await;
                None
            },
            response = receiver => response.ok(),
        }
        .and_then(|value| serde_json::from_value::<ElicitationCreateResponse>(value).ok());
        let (decision, content, meta) = response
            .map_or((ElicitationAction::Cancel, None, None), |response| {
                (response.action, response.content, response.meta)
            });
        if let Err(err) = thread
            .submit(Op::ResolveElicitation {
                server_name: event.server_name,
                request_id: event.id,
                decision,
                content,
                meta,
            })
            .await
        {
            tracing::error!("failed to submit elicitation response: {err}");
        }
    });
}

/// Run a complete Codex session and stream events back to the client.
///
/// On completion (success or error) the function sends the appropriate
/// `tools/call` response so the LLM can continue the conversation.
pub async fn run_codex_tool_session(
    id: RequestId,
    initial_prompt: String,
    config: CodexConfig,
    outgoing: Arc<OutgoingMessageSender>,
    thread_manager: Arc<ThreadManager>,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    let NewThread {
        thread_id,
        thread,
        session_configured,
        ..
    } = match thread_manager.start_thread(config.clone()).await {
        Ok(res) => res,
        Err(e) => {
            let result = CallToolResult::error(vec![Content::text(format!(
                "Failed to start Codex session: {e}"
            ))]);
            outgoing.send_response(id.clone(), result).await;
            return;
        }
    };

    let session_configured_event = Event {
        // Use a fake id value for now.
        id: "".to_string(),
        msg: EventMsg::SessionConfigured(session_configured.clone()),
    };
    outgoing
        .send_event_as_notification(
            &session_configured_event,
            Some(OutgoingNotificationMeta {
                request_id: Some(id.clone()),
                thread_id: Some(thread_id),
            }),
        )
        .await;

    // Use the original MCP request ID as the `sub_id` for the Codex submission so that
    // any events emitted for this tool-call can be correlated with the
    // originating `tools/call` request.
    let sub_id = id.to_string();
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .insert(id.clone(), thread_id);
    let submission = Submission {
        id: sub_id.clone(),
        op: Op::UserInput {
            items: vec![UserInput::Text {
                text: initial_prompt.clone(),
                // MCP tool prompts are plain text with no UI element ranges.
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
    };

    if let Err(e) = thread.submit_with_id(submission).await {
        tracing::error!("Failed to submit initial prompt: {e}");
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            format!("Failed to submit initial prompt: {e}"),
            Some(true),
        );
        outgoing.send_response(id.clone(), result).await;
        // unregister the id so we don't keep it in the map
        running_requests_id_to_codex_uuid.lock().await.remove(&id);
        return;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        id,
        running_requests_id_to_codex_uuid,
    )
    .await;
}

pub async fn run_codex_tool_session_reply(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    prompt: String,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .insert(request_id.clone(), thread_id);
    if let Err(e) = thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt,
                // MCP tool prompts are plain text with no UI element ranges.
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
    {
        tracing::error!("Failed to submit user input: {e}");
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            format!("Failed to submit user input: {e}"),
            Some(true),
        );
        outgoing.send_response(request_id.clone(), result).await;
        // unregister the id so we don't keep it in the map
        running_requests_id_to_codex_uuid
            .lock()
            .await
            .remove(&request_id);
        return;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        request_id,
        running_requests_id_to_codex_uuid,
    )
    .await;
}

async fn run_codex_tool_session_inner(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    run_codex_tool_session_inner_with_source(
        thread_id,
        CodexToolEventSource::Runtime(thread),
        outgoing,
        request_id,
        running_requests_id_to_codex_uuid,
        TURN_COMPLETE_AFTER_STATUS_ERROR_TIMEOUT,
    )
    .await;
}

async fn run_codex_tool_session_inner_with_source(
    thread_id: ThreadId,
    mut event_source: CodexToolEventSource,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
    turn_complete_after_status_error_timeout: Duration,
) {
    let request_id_str = request_id.to_string();
    let elicitation_cancellation = CancellationToken::new();
    let mut pending_turn_error: Option<PendingTurnError> = None;

    // Stream events until the task needs to pause for user interaction or
    // completes.
    loop {
        let Some(next_event) = run_before_pending_error_deadline(
            pending_turn_error.as_ref(),
            event_source.next_event(),
        )
        .await
        else {
            let _ = send_pending_turn_error_response(
                thread_id,
                &mut pending_turn_error,
                &outgoing,
                &request_id,
            )
            .await;
            break;
        };

        let should_continue = match next_event {
            Ok(mut event) => {
                prepare_event_for_publication(
                    &mut event,
                    &mut pending_turn_error,
                    turn_complete_after_status_error_timeout,
                );
                let event_processing = async {
                    outgoing
                        .send_event_as_notification(
                            &event,
                            Some(OutgoingNotificationMeta {
                                request_id: Some(request_id.clone()),
                                thread_id: Some(thread_id),
                            }),
                        )
                        .await;

                    match event.msg {
                        EventMsg::ExecApprovalRequest(ev) => {
                            let approval_id = ev.effective_approval_id();
                            let ExecApprovalRequestEvent {
                                turn_id: _,
                                environment_id: _,
                                started_at_ms: _,
                                command,
                                cwd,
                                cwd_uri: _,
                                call_id,
                                approval_id: _,
                                reason: _,
                                proposed_execpolicy_amendment: _,
                                proposed_network_policy_amendments: _,
                                parsed_cmd,
                                network_approval_context: _,
                                additional_permissions: _,
                                available_decisions: _,
                            } = ev;
                            handle_exec_approval_request(
                                command,
                                cwd.to_path_buf(),
                                outgoing.clone(),
                                event_source.runtime_thread(),
                                request_id.clone(),
                                request_id_str.clone(),
                                event.id.clone(),
                                call_id,
                                approval_id,
                                parsed_cmd,
                                thread_id,
                            )
                            .await;
                            true
                        }
                        EventMsg::PlanDelta(_) => true,
                        EventMsg::Error(err_event) => {
                            if err_event.affects_turn_status() {
                                true
                            } else {
                                // Non-status errors cannot have an authoritative failed
                                // TurnComplete, so preserve the immediate MCP response.
                                let result = create_call_tool_result_with_thread_id(
                                    thread_id,
                                    err_event.message,
                                    Some(true),
                                );
                                outgoing.send_response(request_id.clone(), result).await;
                                false
                            }
                        }
                        EventMsg::Warning(_)
                        | EventMsg::GuardianWarning(_)
                        | EventMsg::ModelVerification(_)
                        | EventMsg::SafetyBuffering(_)
                        | EventMsg::TurnModerationMetadata(_)
                        | EventMsg::GuardianAssessment(_) => true,
                        EventMsg::ElicitationRequest(event) => {
                            forward_elicitation(
                                event,
                                outgoing.clone(),
                                event_source.runtime_thread(),
                                elicitation_cancellation.clone(),
                            )
                            .await;
                            true
                        }
                        EventMsg::TurnAborted(_) => {
                            elicitation_cancellation.cancel();
                            true
                        }
                        EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                            call_id,
                            turn_id: _,
                            started_at_ms: _,
                            reason,
                            grant_root,
                            changes,
                        }) => {
                            handle_patch_approval_request(
                                call_id,
                                reason,
                                grant_root,
                                changes,
                                outgoing.clone(),
                                event_source.runtime_thread(),
                                request_id.clone(),
                                request_id_str.clone(),
                                event.id.clone(),
                                thread_id,
                            )
                            .await;
                            true
                        }
                        EventMsg::TurnComplete(event) => {
                            handle_turn_complete_event(
                                thread_id,
                                event,
                                outgoing.clone(),
                                request_id.clone(),
                            )
                            .await;
                            false
                        }
                        EventMsg::SessionConfigured(_) => {
                            tracing::error!("unexpected SessionConfigured event");
                            true
                        }
                        EventMsg::ThreadGoalUpdated(_) => {
                            // Ignore thread goal metadata updates in MCP tool runner.
                            true
                        }
                        EventMsg::McpStartupUpdate(_) | EventMsg::McpStartupComplete(_) => {
                            // Ignored in MCP tool runner.
                            true
                        }
                        EventMsg::AgentMessage(AgentMessageEvent { .. }) => {
                            // TODO: think how we want to support this in the MCP
                            true
                        }
                        EventMsg::AgentReasoningRawContent(_)
                        | EventMsg::TurnStarted(_)
                        | EventMsg::ThreadSettingsApplied(_)
                        | EventMsg::TokenCount(_)
                        | EventMsg::AgentReasoning(_)
                        | EventMsg::AgentReasoningSectionBreak(_)
                        | EventMsg::McpToolCallBegin(_)
                        | EventMsg::McpToolCallEnd(_)
                        | EventMsg::McpToolCallProgress(_)
                        | EventMsg::ExecCommandBegin(_)
                        | EventMsg::TerminalInteraction(_)
                        | EventMsg::ExecCommandOutputDelta(_)
                        | EventMsg::ExecCommandEnd(_)
                        | EventMsg::StreamError(_)
                        | EventMsg::PatchApplyBegin(_)
                        | EventMsg::PatchApplyUpdated(_)
                        | EventMsg::PatchApplyEnd(_)
                        | EventMsg::TurnDiff(_)
                        | EventMsg::WebSearchBegin(_)
                        | EventMsg::WebSearchEnd(_)
                        | EventMsg::PlanUpdate(_)
                        | EventMsg::UserMessage(_)
                        | EventMsg::ShutdownComplete
                        | EventMsg::ImageGenerationBegin(_)
                        | EventMsg::ImageGenerationEnd(_)
                        | EventMsg::ViewImageToolCall(_)
                        | EventMsg::RawResponseItem(_)
                        | EventMsg::EnteredReviewMode(_)
                        | EventMsg::ItemStarted(_)
                        | EventMsg::ItemCompleted(_)
                        | EventMsg::HookStarted(_)
                        | EventMsg::HookCompleted(_)
                        | EventMsg::AgentMessageContentDelta(_)
                        | EventMsg::ReasoningContentDelta(_)
                        | EventMsg::ReasoningRawContentDelta(_)
                        | EventMsg::ReasoningPolicyUpdated(_)
                        | EventMsg::ReasoningPolicySummary(_)
                        | EventMsg::ExitedReviewMode(_)
                        | EventMsg::RequestUserInput(_)
                        | EventMsg::RequestPermissions(_)
                        | EventMsg::DynamicToolCallRequest(_)
                        | EventMsg::DynamicToolCallResponse(_)
                        | EventMsg::ContextCompacted(_)
                        | EventMsg::ModelReroute(_)
                        | EventMsg::ThreadRolledBack(_)
                        | EventMsg::CollabAgentSpawnBegin(_)
                        | EventMsg::CollabAgentSpawnEnd(_)
                        | EventMsg::CollabAgentInteractionBegin(_)
                        | EventMsg::CollabAgentInteractionEnd(_)
                        | EventMsg::CollabWaitingBegin(_)
                        | EventMsg::CollabWaitingEnd(_)
                        | EventMsg::CollabCloseBegin(_)
                        | EventMsg::CollabCloseEnd(_)
                        | EventMsg::CollabResumeBegin(_)
                        | EventMsg::CollabResumeEnd(_)
                        | EventMsg::SubAgentActivity(_)
                        | EventMsg::DeprecationNotice(_) => {
                            // For now, we do not do anything extra for these
                            // events. Note that the notification above has
                            // already dispatched these events.
                            true
                        }
                    }
                };

                let Some(should_continue) = run_before_pending_error_deadline(
                    pending_turn_error.as_ref(),
                    event_processing,
                )
                .await
                else {
                    let _ = send_pending_turn_error_response(
                        thread_id,
                        &mut pending_turn_error,
                        &outgoing,
                        &request_id,
                    )
                    .await;
                    break;
                };
                should_continue
            }
            Err(e) => {
                let error_text = pending_turn_error.take().map_or_else(
                    || format!("Codex runtime error: {e}"),
                    |pending| pending.error.message,
                );
                let result =
                    create_call_tool_result_with_thread_id(thread_id, error_text, Some(true));
                outgoing.send_response(request_id.clone(), result).await;
                false
            }
        };

        if !should_continue {
            break;
        }
    }
    // Every terminal response and runtime error leaves through this cleanup path.
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .remove(&request_id);
    elicitation_cancellation.cancel();
}

async fn run_before_pending_error_deadline<T>(
    pending_turn_error: Option<&PendingTurnError>,
    operation: impl Future<Output = T>,
) -> Option<T> {
    match pending_turn_error {
        Some(pending) => timeout_at(pending.deadline, operation).await.ok(),
        None => Some(operation.await),
    }
}

fn prepare_event_for_publication(
    event: &mut Event,
    pending_turn_error: &mut Option<PendingTurnError>,
    turn_complete_after_status_error_timeout: Duration,
) {
    if let EventMsg::Error(error) = &event.msg
        && error.affects_turn_status()
        && pending_turn_error.is_none()
    {
        *pending_turn_error = Some(PendingTurnError {
            error: error.clone(),
            deadline: Instant::now() + turn_complete_after_status_error_timeout,
        });
    }

    if let EventMsg::TurnComplete(turn_complete) = &mut event.msg {
        if turn_complete.error.is_none()
            && let Some(pending) = pending_turn_error.as_ref()
        {
            turn_complete.error = Some(pending.error.clone());
        }
        if turn_complete.error.is_some() {
            turn_complete.last_agent_message = None;
            turn_complete.surfaced_result = None;
        }
    }
}

async fn send_pending_turn_error_response(
    thread_id: ThreadId,
    pending_turn_error: &mut Option<PendingTurnError>,
    outgoing: &OutgoingMessageSender,
    request_id: &RequestId,
) -> bool {
    let pending = pending_turn_error
        .take()
        .expect("deadline expiry requires a pending turn error");
    let result =
        create_call_tool_result_with_thread_id(thread_id, pending.error.message, Some(true));
    timeout_at(
        Instant::now() + FALLBACK_RESPONSE_SEND_TIMEOUT,
        outgoing.send_response(request_id.clone(), result),
    )
    .await
    .is_ok()
}

async fn handle_turn_complete_event(
    thread_id: ThreadId,
    event: TurnCompleteEvent,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
) {
    let (text, is_error, surfaced_result) = match event.error {
        Some(error) => (error.message, Some(true), None),
        None => (
            event.last_agent_message.unwrap_or_default(),
            None,
            event.surfaced_result,
        ),
    };
    let result = create_call_tool_result_with_thread_id_and_surfaced_result(
        thread_id,
        text,
        is_error,
        surfaced_result,
    );
    outgoing.send_response(request_id, result).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingMessage;
    use codex_protocol::protocol::ErrorEvent;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    #[test]
    fn call_tool_result_includes_thread_id_in_structured_content() {
        let thread_id = ThreadId::new();
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            "done".to_string(),
            /*is_error*/ None,
        );
        assert_eq!(
            result.structured_content,
            Some(json!({
                "threadId": thread_id,
                "content": "done",
            }))
        );
    }

    #[test]
    fn call_tool_result_preserves_typed_surface_without_synthesizing_text() {
        let thread_id = ThreadId::new();
        let surfaced_result = SurfacedToolResult {
            adapter: "owner".to_string(),
            value: json!({"answer": 42}),
            canonical_message: None,
        };
        let result = create_call_tool_result_with_thread_id_and_surfaced_result(
            thread_id,
            String::new(),
            /*is_error*/ None,
            Some(surfaced_result.clone()),
        );

        assert_eq!(
            result.structured_content,
            Some(json!({
                "threadId": thread_id,
                "content": "",
                "surfacedResult": surfaced_result,
            }))
        );
    }

    #[tokio::test]
    async fn turn_complete_error_event_path_returns_mcp_error_and_hides_success_payload() {
        let thread_id = ThreadId::new();
        let request_id = RequestId::Number(7);
        let (sender, mut receiver) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(sender));
        let mut event = Event {
            id: "event-1".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("success-looking answer".to_string()),
                surfaced_result: Some(SurfacedToolResult {
                    adapter: "success-adapter".to_string(),
                    value: json!({"result": "success-looking payload"}),
                    canonical_message: Some("success-looking canonical message".to_string()),
                }),
                error: Some(ErrorEvent {
                    message: "completion proof is missing".to_string(),
                    codex_error_info: None,
                }),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: None,
            }),
        };

        prepare_event_for_publication(
            &mut event,
            &mut None,
            TURN_COMPLETE_AFTER_STATUS_ERROR_TIMEOUT,
        );
        outgoing
            .send_event_as_notification(
                &event,
                Some(OutgoingNotificationMeta {
                    request_id: Some(request_id.clone()),
                    thread_id: Some(thread_id),
                }),
            )
            .await;
        let EventMsg::TurnComplete(turn_complete) = event.msg else {
            unreachable!("test event is turn-complete")
        };

        handle_turn_complete_event(thread_id, turn_complete, outgoing, request_id.clone()).await;

        let Some(OutgoingMessage::Notification(notification)) = receiver.recv().await else {
            panic!("turn-complete event should emit an MCP notification first");
        };
        let notification_json = serde_json::to_string(&notification)
            .expect("MCP notification should serialize for assertion");
        assert!(!notification_json.contains("success-looking"));

        let Some(OutgoingMessage::Response(response)) = receiver.recv().await else {
            panic!("turn-complete event should emit an MCP tools/call response");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(
            response.result,
            json!({
                "content": [{
                    "type": "text",
                    "text": "completion proof is missing",
                }],
                "isError": true,
                "structuredContent": {
                    "threadId": thread_id,
                    "content": "completion proof is missing",
                },
            })
        );
    }

    #[tokio::test]
    async fn pending_status_error_prevents_later_successful_turn_complete() {
        let thread_id = ThreadId::new();
        let request_id = RequestId::Number(8);
        let (sender, mut receiver) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(sender));
        let mut pending_turn_error = None;
        let mut error_event = Event {
            id: "status-error".to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "the turn failed before completion".to_string(),
                codex_error_info: None,
            }),
        };
        prepare_event_for_publication(
            &mut error_event,
            &mut pending_turn_error,
            TURN_COMPLETE_AFTER_STATUS_ERROR_TIMEOUT,
        );
        assert!(pending_turn_error.is_some());

        let mut completion_event = Event {
            id: "turn-complete".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("success-looking answer".to_string()),
                surfaced_result: Some(SurfacedToolResult {
                    adapter: "success-adapter".to_string(),
                    value: json!({"result": "success-looking payload"}),
                    canonical_message: Some("success-looking canonical message".to_string()),
                }),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: None,
            }),
        };
        prepare_event_for_publication(
            &mut completion_event,
            &mut pending_turn_error,
            TURN_COMPLETE_AFTER_STATUS_ERROR_TIMEOUT,
        );
        outgoing
            .send_event_as_notification(
                &completion_event,
                Some(OutgoingNotificationMeta {
                    request_id: Some(request_id.clone()),
                    thread_id: Some(thread_id),
                }),
            )
            .await;
        let EventMsg::TurnComplete(turn_complete) = completion_event.msg else {
            unreachable!("test event is turn-complete")
        };
        assert_eq!(
            turn_complete
                .error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some("the turn failed before completion")
        );
        assert_eq!(turn_complete.last_agent_message, None);
        assert_eq!(turn_complete.surfaced_result, None);

        handle_turn_complete_event(thread_id, turn_complete, outgoing, request_id.clone()).await;

        let Some(OutgoingMessage::Notification(notification)) = receiver.recv().await else {
            panic!("turn-complete event should emit an MCP notification first");
        };
        let notification_json = serde_json::to_string(&notification)
            .expect("MCP notification should serialize for assertion");
        assert!(notification_json.contains("the turn failed before completion"));
        assert!(!notification_json.contains("success-looking"));

        let Some(OutgoingMessage::Response(response)) = receiver.recv().await else {
            panic!("turn-complete event should emit an MCP tools/call response");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(response.result.get("isError"), Some(&json!(true)));
        assert_eq!(
            response
                .result
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str),
            Some("the turn failed before completion")
        );
        assert!(
            response
                .result
                .pointer("/structuredContent/surfacedResult")
                .is_none()
        );
    }

    #[tokio::test]
    async fn fallback_deadline_bounds_slow_event_handling() {
        let thread_id = ThreadId::new();
        let request_id = RequestId::Number(9);
        let (sender, mut receiver) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(sender));
        let mut pending_turn_error = Some(PendingTurnError {
            error: ErrorEvent {
                message: "missing failed TurnComplete".to_string(),
                codex_error_info: None,
            },
            deadline: Instant::now() + Duration::from_millis(25),
        });
        let event_was_received = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let processing_result = run_before_pending_error_deadline(pending_turn_error.as_ref(), {
            let event_was_received = event_was_received.clone();
            let handler_completed = handler_completed.clone();
            async move {
                event_was_received.store(true, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(60)).await;
                handler_completed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await;

        assert!(
            processing_result.is_none(),
            "the absolute deadline must win"
        );
        assert!(event_was_received.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!handler_completed.load(std::sync::atomic::Ordering::SeqCst));

        let sent = send_pending_turn_error_response(
            thread_id,
            &mut pending_turn_error,
            &outgoing,
            &request_id,
        )
        .await;
        assert!(sent);
        let Some(OutgoingMessage::Response(response)) = receiver.recv().await else {
            panic!("deadline fallback should emit an MCP tools/call response");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(response.result.get("isError"), Some(&json!(true)));
        assert_eq!(
            response
                .result
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str),
            Some("missing failed TurnComplete")
        );
    }

    #[tokio::test]
    async fn fallback_response_does_not_hang_when_outgoing_queue_is_full() {
        let thread_id = ThreadId::new();
        let request_id = RequestId::Number(10);
        let (outgoing_sender, mut outgoing_receiver) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(outgoing_sender));
        outgoing
            .send_response(RequestId::Number(999), json!({"prefill": true}))
            .await;

        let (event_sender, event_receiver) = mpsc::channel::<CodexResult<Event>>(1);
        event_sender
            .send(Ok(status_error_event("queue pressure failure")))
            .await
            .expect("scripted status error should be queued");

        let running_requests =
            Arc::new(Mutex::new(HashMap::from([(request_id.clone(), thread_id)])));
        tokio::time::timeout(
            Duration::from_secs(1),
            run_codex_tool_session_inner_with_source(
                thread_id,
                CodexToolEventSource::Scripted(event_receiver),
                outgoing,
                request_id.clone(),
                running_requests.clone(),
                Duration::from_millis(25),
            ),
        )
        .await
        .expect("full outgoing queue must not prevent bounded fallback cleanup");

        assert!(!running_requests.lock().await.contains_key(&request_id));
        let Some(OutgoingMessage::Response(prefill)) = outgoing_receiver.recv().await else {
            panic!("the prefilled response should remain first in the queue");
        };
        assert_eq!(prefill.id, RequestId::Number(999));
        assert!(
            matches!(
                outgoing_receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
            ),
            "the bounded fallback must be canceled rather than hang on a full queue"
        );

        drop(event_sender);
    }

    #[tokio::test]
    async fn runtime_loop_missing_turn_complete_returns_bounded_mcp_error() {
        let thread_id = ThreadId::new();
        let request_id = RequestId::Number(11);
        let (outgoing_sender, mut outgoing_receiver) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(outgoing_sender));
        let (event_sender, event_receiver) = mpsc::channel::<CodexResult<Event>>(1);
        event_sender
            .send(Ok(status_error_event("missing terminal failure")))
            .await
            .expect("scripted status error should be queued");

        let running_requests =
            Arc::new(Mutex::new(HashMap::from([(request_id.clone(), thread_id)])));
        tokio::time::timeout(
            Duration::from_secs(1),
            run_codex_tool_session_inner_with_source(
                thread_id,
                CodexToolEventSource::Scripted(event_receiver),
                outgoing,
                request_id.clone(),
                running_requests.clone(),
                Duration::from_millis(25),
            ),
        )
        .await
        .expect("missing TurnComplete must reach a bounded terminal response");

        let Some(OutgoingMessage::Notification(notification)) = outgoing_receiver.recv().await
        else {
            panic!("the runtime loop should publish the status error notification");
        };
        let notification_json = serde_json::to_value(notification)
            .expect("status error notification should serialize for assertion");
        assert_eq!(
            notification_json.pointer("/params/msg/message"),
            Some(&json!("missing terminal failure"))
        );

        let Some(OutgoingMessage::Response(response)) = outgoing_receiver.recv().await else {
            panic!("the runtime loop should publish the bounded MCP error response");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(response.result.get("isError"), Some(&json!(true)));
        assert_eq!(
            response
                .result
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str),
            Some("missing terminal failure")
        );
        assert!(!running_requests.lock().await.contains_key(&request_id));

        drop(event_sender);
    }

    fn status_error_event(message: &str) -> Event {
        Event {
            id: "status-error".to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: message.to_string(),
                codex_error_info: None,
            }),
        }
    }
}
