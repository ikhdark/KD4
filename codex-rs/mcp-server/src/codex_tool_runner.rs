//! Asynchronous worker that executes a **Codex** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.

use std::collections::HashMap;
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
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SurfacedToolResult;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::RequestId;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct RunningRequest {
    pub(crate) thread_id: Option<ThreadId>,
    pub(crate) turn_id: String,
    pub(crate) cancellation: CancellationToken,
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
    let pending_request = match outgoing
        .send_request("elicitation/create", Some(params))
        .await
    {
        Ok(pending_request) => pending_request,
        Err(err) => {
            tracing::error!("failed to request elicitation: {err:?}");
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
    };
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
    request: RunningRequest,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, RunningRequest>>>,
) {
    let NewThread {
        thread_id,
        thread,
        session_configured,
        ..
    } = match thread_manager.start_thread(config).await {
        Ok(res) => res,
        Err(e) => {
            let result = CallToolResult::error(vec![Content::text(format!(
                "Failed to start Codex session: {e}"
            ))]);
            outgoing.send_response(id.clone(), result).await;
            running_requests_id_to_codex_uuid.lock().await.remove(&id);
            return;
        }
    };

    // Attach the thread without replacing the token registered at admission.
    if let Some(registered) = running_requests_id_to_codex_uuid.lock().await.get_mut(&id) {
        registered.thread_id = Some(thread_id);
    }
    if request.cancellation.is_cancelled() {
        outgoing
            .send_response(
                id.clone(),
                create_call_tool_result_with_thread_id(
                    thread_id,
                    "Codex request cancelled during startup.".to_string(),
                    Some(true),
                ),
            )
            .await;
        running_requests_id_to_codex_uuid.lock().await.remove(&id);
        return;
    }

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

    let sub_id = request.turn_id.clone();
    if request.cancellation.is_cancelled() {
        outgoing
            .send_response(
                id.clone(),
                create_call_tool_result_with_thread_id(
                    thread_id,
                    "Codex request cancelled during startup.".to_string(),
                    Some(true),
                ),
            )
            .await;
        running_requests_id_to_codex_uuid.lock().await.remove(&id);
        return;
    }
    let submission = Op::UserInput {
        items: vec![UserInput::Text {
            text: initial_prompt.clone(),
            // MCP tool prompts are plain text with no UI element ranges.
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: Default::default(),
    };

    if let Err(e) = thread
        .submit_user_input_with_reserved_turn_id(sub_id, submission, None, None)
        .await
    {
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
    if request.cancellation.is_cancelled() {
        thread.interrupt_turn_if_active(&request.turn_id).await;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        id,
        request.turn_id,
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
    request: RunningRequest,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, RunningRequest>>>,
) {
    if request.cancellation.is_cancelled() {
        outgoing
            .send_response(
                request_id.clone(),
                create_call_tool_result_with_thread_id(
                    thread_id,
                    "Codex request cancelled before turn start.".to_string(),
                    Some(true),
                ),
            )
            .await;
        running_requests_id_to_codex_uuid
            .lock()
            .await
            .remove(&request_id);
        return;
    }
    if let Err(e) = thread
        .submit_user_input_with_reserved_turn_id(
            request.turn_id.clone(),
            Op::UserInput {
                items: vec![UserInput::Text {
                    text: prompt,
                    // MCP tool prompts are plain text with no UI element ranges.
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            },
            None,
            None,
        )
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
    if request.cancellation.is_cancelled() {
        thread.interrupt_turn_if_active(&request.turn_id).await;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        request_id,
        request.turn_id,
        running_requests_id_to_codex_uuid,
    )
    .await;
}

/// Streams events until this request's turn reaches its terminal event. Error
/// events do not end the turn; core reports terminal errors on `TurnComplete`.
/// Leaving that terminal event queued would let a later reply on this thread
/// consume it as its own result.
async fn run_codex_tool_session_inner(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    turn_id: String,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, RunningRequest>>>,
) {
    let request_id_str = request_id.to_string();
    let elicitation_cancellation = CancellationToken::new();
    let _elicitation_drop_guard = elicitation_cancellation.clone().drop_guard();
    let mut unsupported_interaction = None;

    // Stream events until the task needs to pause for user interaction or
    // completes.
    loop {
        match thread.next_event().await {
            Ok(event) => {
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
                    EventMsg::RequestUserInput(ev) => {
                        unsupported_interaction =
                            Some("request_user_input is not supported by the MCP server.");
                        thread.interrupt_turn_if_active(&ev.turn_id).await;
                    }
                    EventMsg::RequestPermissions(ev) => {
                        unsupported_interaction =
                            Some("request_permissions is not supported by the MCP server.");
                        thread.interrupt_turn_if_active(&ev.turn_id).await;
                    }
                    EventMsg::DynamicToolCallRequest(ev) => {
                        unsupported_interaction =
                            Some("Dynamic tool execution is not supported by the MCP server.");
                        thread.interrupt_turn_if_active(&ev.turn_id).await;
                    }
                    EventMsg::ExecApprovalRequest(ev) => {
                        handle_exec_approval_request(
                            ev,
                            outgoing.clone(),
                            thread.clone(),
                            request_id_str.clone(),
                            event.id.clone(),
                            thread_id,
                            elicitation_cancellation.clone(),
                        )
                        .await;
                        continue;
                    }
                    EventMsg::PlanDelta(_) | EventMsg::Error(_) => {
                        continue;
                    }
                    EventMsg::Warning(_)
                    | EventMsg::ModelVerification(_)
                    | EventMsg::SafetyBuffering(_)
                    | EventMsg::TurnModerationMetadata(_) => {
                        continue;
                    }
                    EventMsg::ElicitationRequest(event) => {
                        forward_elicitation(
                            event,
                            outgoing.clone(),
                            thread.clone(),
                            elicitation_cancellation.clone(),
                        )
                        .await;
                        continue;
                    }
                    EventMsg::TurnAborted(aborted)
                        if aborted
                            .turn_id
                            .as_ref()
                            .is_some_and(|aborted_turn_id| *aborted_turn_id != turn_id) => {}
                    EventMsg::TurnAborted(_) => {
                        elicitation_cancellation.cancel();
                        let result = create_call_tool_result_with_thread_id(
                            thread_id,
                            unsupported_interaction
                                .unwrap_or("Turn aborted.")
                                .to_string(),
                            Some(true),
                        );
                        outgoing.send_response(request_id.clone(), result).await;
                        break;
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
                            thread.clone(),
                            request_id_str.clone(),
                            event.id.clone(),
                            thread_id,
                            elicitation_cancellation.clone(),
                        )
                        .await;
                        continue;
                    }
                    EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: completed_turn_id,
                        ..
                    }) if completed_turn_id != turn_id => {}
                    EventMsg::TurnComplete(TurnCompleteEvent {
                        last_agent_message,
                        surfaced_result,
                        error,
                        ..
                    }) => {
                        // Always respond in tools/call's expected shape, and include the thread id so the client can resume.
                        let result = match error {
                            Some(error) => create_call_tool_result_with_thread_id(
                                thread_id,
                                error.message,
                                Some(true),
                            ),
                            None => create_call_tool_result_with_thread_id_and_surfaced_result(
                                thread_id,
                                last_agent_message.unwrap_or_default(),
                                /*is_error*/ None,
                                surfaced_result,
                            ),
                        };
                        outgoing.send_response(request_id.clone(), result).await;
                        // unregister the id so we don't keep it in the map
                        running_requests_id_to_codex_uuid
                            .lock()
                            .await
                            .remove(&request_id);
                        break;
                    }
                    EventMsg::SessionConfigured(_) => {
                        tracing::error!("unexpected SessionConfigured event");
                    }
                    EventMsg::ThreadGoalUpdated(_) => {
                        // Ignore thread goal metadata updates in MCP tool runner.
                    }
                    EventMsg::McpStartupUpdate(_) | EventMsg::McpStartupComplete(_) => {
                        // Ignored in MCP tool runner.
                    }
                    EventMsg::AgentMessage(AgentMessageEvent { .. }) => {
                        // TODO: think how we want to support this in the MCP
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
                    | EventMsg::ExitedReviewMode(_)
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
                        // events. Note that
                        // send(codex_event_to_notification(&event)) above has
                        // already dispatched these events as notifications,
                        // though we may want to do give different treatment to
                        // individual events in the future.
                    }
                }
            }
            Err(e) => {
                let result = create_call_tool_result_with_thread_id(
                    thread_id,
                    format!("Codex runtime error: {e}"),
                    Some(true),
                );
                outgoing.send_response(request_id.clone(), result).await;
                break;
            }
        }
    }
    elicitation_cancellation.cancel();
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .remove(&request_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
}
