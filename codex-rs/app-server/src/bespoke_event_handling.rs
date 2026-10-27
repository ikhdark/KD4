use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ClientRequestResult;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::request_processors::populate_thread_turns_from_history;
use crate::request_processors::thread_from_stored_thread;
use crate::request_processors::thread_settings_from_core_snapshot;
use crate::server_request_error::is_turn_transition_server_request_error;
use crate::thread_state::ThreadState;
use crate::thread_state::TurnSummary;
use crate::thread_state::resolve_server_request_on_thread_listener;
use crate::thread_status::ThreadWatchActiveGuard;
use crate::thread_status::ThreadWatchManager;
use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
use codex_app_server_protocol::AdditionalPermissionProfile as V2AdditionalPermissionProfile;
use codex_app_server_protocol::CodexErrorInfo as V2CodexErrorInfo;
use codex_app_server_protocol::CommandAction as V2ParsedCommand;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DeprecationNoticeNotification;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::ExecPolicyAmendment as V2ExecPolicyAmendment;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::GrantedPermissionProfile as V2GrantedPermissionProfile;
use codex_app_server_protocol::HookCompletedNotification;
use codex_app_server_protocol::HookStartedNotification;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::McpServerElicitationRequestParams;
use codex_app_server_protocol::McpServerElicitationRequestResponse;
use codex_app_server_protocol::McpServerStartupCompletedNotification;
use codex_app_server_protocol::McpServerStartupFailure;
use codex_app_server_protocol::McpServerStartupState;
use codex_app_server_protocol::McpServerStatusUpdatedNotification;
use codex_app_server_protocol::McpToolCallProgressNotification;
use codex_app_server_protocol::ModelReroutedNotification;
use codex_app_server_protocol::ModelSafetyBufferingUpdatedNotification;
use codex_app_server_protocol::ModelVerificationNotification;
use codex_app_server_protocol::NetworkApprovalContext as V2NetworkApprovalContext;
use codex_app_server_protocol::NetworkPolicyAmendment as V2NetworkPolicyAmendment;
use codex_app_server_protocol::NetworkPolicyRuleAction as V2NetworkPolicyRuleAction;
use codex_app_server_protocol::PermissionsRequestApprovalParams;
use codex_app_server_protocol::PermissionsRequestApprovalResponse;
use codex_app_server_protocol::RawResponseItemCompletedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::ToolRequestUserInputOption;
use codex_app_server_protocol::ToolRequestUserInputParams;
use codex_app_server_protocol::ToolRequestUserInputQuestion;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnDiffUpdatedNotification;
use codex_app_server_protocol::TurnError;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnModerationMetadataNotification;
use codex_app_server_protocol::TurnPlanStep;
use codex_app_server_protocol::TurnPlanUpdatedNotification;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnTiming;
use codex_app_server_protocol::WarningNotification;
use codex_app_server_protocol::command_display_string;
use codex_app_server_protocol::item_event_to_server_notification;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool as CoreCollabAgentTool;
use codex_protocol::items::TurnItem as CoreTurnItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::CodexErrorInfo as CoreCodexErrorInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnDiffEvent;
use codex_protocol::request_permissions::PermissionGrantScope as CorePermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile as CoreRequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse as CoreRequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputAnswer as CoreRequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse as CoreRequestUserInputResponse;
use codex_sandboxing::policy_transforms::intersect_uri_permission_profiles;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::error;

enum CommandExecutionApprovalPresentation {
    Network(V2NetworkApprovalContext),
    Command(CommandExecutionCompletionItem),
}

fn mcp_tool_call_progress_notification(
    conversation_id: ThreadId,
    turn_id: String,
    item_id: String,
    message: String,
    progress: Option<f64>,
    total: Option<f64>,
) -> ServerNotification {
    ServerNotification::McpToolCallProgress(McpToolCallProgressNotification {
        thread_id: conversation_id.to_string(),
        turn_id,
        item_id,
        message,
        progress,
        total,
    })
}

#[derive(Debug, PartialEq)]
struct CommandExecutionCompletionItem {
    command: String,
    cwd: LegacyAppPathString,
    command_actions: Vec<V2ParsedCommand>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_bespoke_event_handling(
    event: Event,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
    thread_manager: Arc<ThreadManager>,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_state: Arc<tokio::sync::Mutex<ThreadState>>,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<crate::request_processors::ThreadListState>,
    fallback_model_provider: String,
) {
    let Event {
        id: event_turn_id,
        msg,
    } = event;
    match msg {
        EventMsg::TurnPhaseChanged(payload) => {
            let is_current = thread_state
                .lock()
                .await
                .active_turn_id()
                .is_some_and(|turn_id| turn_id == payload.turn_id);
            if is_current {
                thread_watch_manager
                    .note_harness_phases(&conversation_id.to_string(), payload.phases)
                    .await;
            }
        }
        EventMsg::TurnStarted(payload) => {
            // While not technically necessary as it was already done on TurnComplete, be extra cautios and abort any pending server requests.
            outgoing.abort_pending_server_requests().await;
            thread_watch_manager
                .note_turn_started(&conversation_id.to_string())
                .await;
            let turn = {
                let state = thread_state.lock().await;
                let mut turn = state.active_turn_snapshot().unwrap_or_else(|| Turn {
                    id: payload.turn_id.clone(),
                    items: Vec::new(),
                    items_view: TurnItemsView::NotLoaded,
                    error: None,
                    status: TurnStatus::InProgress,
                    started_at: payload.started_at,
                    completed_at: None,
                    duration_ms: None,
                    timing: None,
                    surfaced_result: None,
                });
                turn.items.clear();
                turn.items_view = TurnItemsView::NotLoaded;
                turn
            };
            let notification = TurnStartedNotification {
                thread_id: conversation_id.to_string(),
                turn,
            };
            outgoing
                .send_server_notification(ServerNotification::TurnStarted(notification))
                .await;
        }
        EventMsg::TurnComplete(turn_complete_event) => {
            let (is_current, turn_failed) = {
                let state = thread_state.lock().await;
                (
                    state.is_current_summary_turn(&event_turn_id),
                    turn_complete_event.error.is_some()
                        || state
                            .turn_summary_for(&event_turn_id)
                            .is_some_and(|summary| summary.last_error.is_some()),
                )
            };
            if is_current {
                outgoing.abort_pending_server_requests().await;
            }
            respond_to_pending_interrupts(&thread_state, &outgoing, &event_turn_id).await;
            handle_turn_complete(
                conversation_id,
                event_turn_id,
                turn_complete_event,
                &outgoing,
                &thread_state,
            )
            .await;
            // Zero running turns now implies terminal dispatch has registered
            // its physical-write receipts. Host shutdown settles those before
            // closing established transports.
            if is_current {
                thread_watch_manager
                    .note_turn_completed(&conversation_id.to_string(), turn_failed)
                    .await;
            }
        }
        EventMsg::McpStartupUpdate(update) => {
            let (status, error, failure_reason) = match update.status {
                codex_protocol::protocol::McpStartupStatus::Starting => {
                    (McpServerStartupState::Starting, None, None)
                }
                codex_protocol::protocol::McpStartupStatus::Ready => {
                    (McpServerStartupState::Ready, None, None)
                }
                codex_protocol::protocol::McpStartupStatus::Failed { error, reason } => (
                    McpServerStartupState::Failed,
                    Some(error),
                    reason.map(Into::into),
                ),
                codex_protocol::protocol::McpStartupStatus::Cancelled => {
                    (McpServerStartupState::Cancelled, None, None)
                }
            };
            let notification = McpServerStatusUpdatedNotification {
                thread_id: Some(conversation_id.to_string()),
                name: update.server,
                status,
                error,
                failure_reason,
            };
            outgoing
                .send_server_notification(ServerNotification::McpServerStatusUpdated(notification))
                .await;
        }
        EventMsg::McpStartupComplete(completed) => {
            let notification = McpServerStartupCompletedNotification {
                thread_id: Some(conversation_id.to_string()),
                ready: completed.ready,
                failed: completed
                    .failed
                    .into_iter()
                    .map(|failure| McpServerStartupFailure {
                        server: failure.server,
                        error: failure.error,
                    })
                    .collect(),
                cancelled: completed.cancelled,
            };
            outgoing
                .send_server_notification(ServerNotification::McpServerStartupCompleted(
                    notification,
                ))
                .await;
        }
        EventMsg::Warning(warning_event) => {
            let notification = WarningNotification {
                thread_id: Some(conversation_id.to_string()),
                message: warning_event.message,
            };
            outgoing
                .send_server_notification(ServerNotification::Warning(notification))
                .await;
        }
        EventMsg::ModelReroute(event) => {
            let notification = ModelReroutedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                from_model: event.from_model,
                to_model: event.to_model,
                reason: event.reason.into(),
            };
            outgoing
                .send_server_notification(ServerNotification::ModelRerouted(notification))
                .await;
        }
        EventMsg::ModelVerification(event) => {
            let notification = ModelVerificationNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                verifications: event.verifications.into_iter().map(Into::into).collect(),
            };
            outgoing
                .send_server_notification(ServerNotification::ModelVerification(notification))
                .await;
        }
        EventMsg::TurnModerationMetadata(event) => {
            let notification = TurnModerationMetadataNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                metadata: event.metadata,
            };
            outgoing
                .send_server_notification(ServerNotification::TurnModerationMetadata(notification))
                .await;
        }
        EventMsg::SafetyBuffering(event) => {
            let notification = ModelSafetyBufferingUpdatedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                model: event.model,
                use_cases: event.use_cases,
                reasons: event.reasons,
                show_buffering_ui: event.show_buffering_ui,
                faster_model: event.faster_model,
            };
            outgoing
                .send_server_notification(ServerNotification::ModelSafetyBufferingUpdated(
                    notification,
                ))
                .await;
        }
        EventMsg::ApplyPatchApprovalRequest(event) => {
            let permission_guard = thread_watch_manager
                .note_permission_requested(&conversation_id.to_string())
                .await;
            let item_id = event.call_id.clone();

            let params = FileChangeRequestApprovalParams {
                thread_id: conversation_id.to_string(),
                turn_id: event.turn_id.clone(),
                item_id: item_id.clone(),
                started_at_ms: event.started_at_ms,
                reason: event.reason.clone(),
                grant_root: event.grant_root.clone(),
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::FileChangeRequestApproval(params))
                .await;
            tokio::spawn(async move {
                on_file_change_request_approval_response(
                    item_id,
                    pending_request_id,
                    rx,
                    conversation,
                    thread_state.clone(),
                    permission_guard,
                )
                .await;
            });
        }
        EventMsg::ExecApprovalRequest(ev) => {
            let permission_guard = thread_watch_manager
                .note_permission_requested(&conversation_id.to_string())
                .await;
            let available_decisions = ev
                .effective_available_decisions()
                .into_iter()
                .map(CommandExecutionApprovalDecision::from)
                .collect::<Vec<_>>();
            let ExecApprovalRequestEvent {
                call_id,
                approval_id,
                turn_id,
                environment_id,
                started_at_ms,
                command,
                cwd,
                cwd_uri,
                reason,
                network_approval_context,
                proposed_execpolicy_amendment,
                proposed_network_policy_amendments,
                additional_permissions,
                parsed_cmd,
                ..
            } = ev;
            let completion_cwd = cwd_uri.unwrap_or_else(|| PathUri::from_abs_path(&cwd));
            let command_actions = codex_app_server_protocol::command_actions_for_path_uri(
                &parsed_cmd,
                &completion_cwd,
            );
            let presentation = if let Some(network_approval_context) =
                network_approval_context.map(V2NetworkApprovalContext::from)
            {
                CommandExecutionApprovalPresentation::Network(network_approval_context)
            } else {
                let command_string = command_display_string(&command);
                let completion_item = CommandExecutionCompletionItem {
                    command: command_string,
                    cwd: completion_cwd.into(),
                    command_actions: command_actions.clone(),
                };
                CommandExecutionApprovalPresentation::Command(completion_item)
            };
            let (network_approval_context, command, cwd, command_actions, completion_item) =
                match presentation {
                    CommandExecutionApprovalPresentation::Network(network_approval_context) => {
                        (Some(network_approval_context), None, None, None, None)
                    }
                    CommandExecutionApprovalPresentation::Command(completion_item) => (
                        None,
                        Some(completion_item.command.clone()),
                        Some(completion_item.cwd.clone()),
                        Some(completion_item.command_actions.clone()),
                        Some(completion_item),
                    ),
                };
            if approval_id.is_none()
                && let Some(completion_item) = completion_item.as_ref()
            {
                start_command_execution_item(
                    &conversation_id,
                    event_turn_id.clone(),
                    call_id.clone(),
                    completion_item.command.clone(),
                    completion_item.cwd.clone(),
                    completion_item.command_actions.clone(),
                    CommandExecutionSource::Agent,
                    &outgoing,
                    &thread_state,
                )
                .await;
            }
            let proposed_execpolicy_amendment_v2 =
                proposed_execpolicy_amendment.map(V2ExecPolicyAmendment::from);
            let proposed_network_policy_amendments_v2 =
                proposed_network_policy_amendments.map(|amendments| {
                    amendments
                        .into_iter()
                        .map(V2NetworkPolicyAmendment::from)
                        .collect()
                });
            let additional_permissions =
                additional_permissions.map(V2AdditionalPermissionProfile::from);

            let params = CommandExecutionRequestApprovalParams {
                thread_id: conversation_id.to_string(),
                turn_id: turn_id.clone(),
                item_id: call_id.clone(),
                started_at_ms,
                approval_id: approval_id.clone(),
                environment_id,
                reason,
                network_approval_context,
                command,
                cwd,
                command_actions,
                additional_permissions,
                proposed_execpolicy_amendment: proposed_execpolicy_amendment_v2,
                proposed_network_policy_amendments: proposed_network_policy_amendments_v2,
                available_decisions: Some(available_decisions),
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
                    params,
                ))
                .await;
            tokio::spawn(async move {
                on_command_execution_request_approval_response(
                    event_turn_id,
                    conversation_id,
                    approval_id,
                    call_id,
                    completion_item,
                    pending_request_id,
                    rx,
                    conversation,
                    outgoing,
                    thread_state.clone(),
                    permission_guard,
                )
                .await;
            });
        }
        EventMsg::RequestUserInput(request) => {
            let user_input_guard = thread_watch_manager
                .note_user_input_requested(&conversation_id.to_string())
                .await;
            let questions = request
                .questions
                .into_iter()
                .map(|question| ToolRequestUserInputQuestion {
                    id: question.id,
                    header: question.header,
                    question: question.question,
                    is_other: question.is_other,
                    is_secret: question.is_secret,
                    options: question.options.map(|options| {
                        options
                            .into_iter()
                            .map(|option| ToolRequestUserInputOption {
                                label: option.label,
                                description: option.description,
                            })
                            .collect()
                    }),
                })
                .collect();
            let call_id = request.call_id.clone();
            let request_turn_id = request.turn_id.clone();
            let params = ToolRequestUserInputParams {
                thread_id: conversation_id.to_string(),
                turn_id: request.turn_id,
                item_id: request.call_id,
                questions,
                auto_resolution_ms: request.auto_resolution_ms,
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::ToolRequestUserInput(params))
                .await;
            tokio::spawn(async move {
                on_request_user_input_response(
                    request_turn_id,
                    call_id,
                    pending_request_id,
                    rx,
                    conversation,
                    thread_state,
                    user_input_guard,
                )
                .await;
            });
        }
        EventMsg::ElicitationRequest(request) => {
            let permission_guard = thread_watch_manager
                .note_permission_requested(&conversation_id.to_string())
                .await;
            let turn_id = match request.turn_id.clone() {
                Some(turn_id) => Some(turn_id),
                None => {
                    let state = thread_state.lock().await;
                    state.active_turn_id().map(str::to_owned)
                }
            };
            let server_name = request.server_name.clone();
            let request_body = match request.request.try_into() {
                Ok(request_body) => request_body,
                Err(err) => {
                    error!(
                        error = %err,
                        server_name,
                        request_id = ?request.id,
                        "failed to parse typed MCP elicitation schema"
                    );
                    if let Err(err) = conversation
                        .submit(Op::ResolveElicitation {
                            server_name: request.server_name,
                            request_id: request.id,
                            decision: codex_protocol::approvals::ElicitationAction::Cancel,
                            content: None,
                            meta: None,
                        })
                        .await
                    {
                        error!("failed to submit ResolveElicitation: {err}");
                    }
                    return;
                }
            };
            let params = McpServerElicitationRequestParams {
                thread_id: conversation_id.to_string(),
                turn_id,
                server_name: request.server_name.clone(),
                request: request_body,
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::McpServerElicitationRequest(params))
                .await;
            tokio::spawn(async move {
                on_mcp_server_elicitation_response(
                    request.server_name,
                    request.id,
                    pending_request_id,
                    rx,
                    conversation,
                    thread_state,
                    permission_guard,
                )
                .await;
            });
        }
        EventMsg::RequestPermissions(request) => {
            let permission_guard = thread_watch_manager
                .note_permission_requested(&conversation_id.to_string())
                .await;
            let requested_permissions = request.permissions.clone();
            let config_snapshot = conversation.config_snapshot().await;
            let request_cwd = request
                .cwd
                .clone()
                .unwrap_or_else(|| config_snapshot.cwd().clone());
            let request_cwd_uri = if let Some(cwd_uri) = request
                .cwd_uri
                .clone()
                .or_else(|| request.cwd.as_ref().map(PathUri::from_abs_path))
            {
                cwd_uri
            } else {
                config_snapshot
                    .environment_selections()
                    .first()
                    .map(|environment| environment.cwd.clone())
                    .unwrap_or_else(|| PathUri::from_abs_path(config_snapshot.cwd()))
            };
            let params = PermissionsRequestApprovalParams {
                thread_id: conversation_id.to_string(),
                turn_id: request.turn_id.clone(),
                item_id: request.call_id.clone(),
                environment_id: request.environment_id.clone(),
                started_at_ms: request.started_at_ms,
                cwd: request_cwd,
                cwd_uri: request_cwd_uri.clone(),
                reason: request.reason,
                permissions: request.permissions.into(),
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::PermissionsRequestApproval(params))
                .await;
            let pending_response = PendingRequestPermissionsResponse {
                call_id: request.call_id,
                conversation_id,
                turn_id: request.turn_id,
                requested_permissions,
                request_cwd: request_cwd_uri,
                pending_request_id,
                outgoing,
                receiver: rx,
                request_permissions_guard: permission_guard,
            };
            tokio::spawn(async move {
                on_request_permissions_response(pending_response, conversation, thread_state).await;
            });
        }
        EventMsg::DynamicToolCallRequest(_)
        | EventMsg::DynamicToolCallResponse(_)
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
        | EventMsg::ExecCommandBegin(_)
        | EventMsg::ExecCommandEnd(_)
        | EventMsg::EnteredReviewMode(_)
        | EventMsg::ExitedReviewMode(_) => {
            // Deprecated item lifecycle events are still fanned out for raw-event and rollout
            // compatibility consumers.
            // App-server v2 receives TurnItem lifecycle instead, and dispatches dynamic tool
            // requests from DynamicToolCall starts.
        }
        EventMsg::McpToolCallProgress(event) => {
            outgoing
                .send_server_notification(mcp_tool_call_progress_notification(
                    conversation_id,
                    event_turn_id,
                    event.call_id,
                    event.message,
                    event.progress,
                    event.total,
                ))
                .await;
        }
        EventMsg::McpToolCallBegin(_) | EventMsg::McpToolCallEnd(_) => {
            // Deprecated MCP tool-call events are still fanned out for raw-event and rollout
            // compatibility consumers.
            // App-server v2 receives the canonical TurnItem::McpToolCall lifecycle instead.
        }
        msg @ (EventMsg::AgentMessageContentDelta(_)
        | EventMsg::PlanDelta(_)
        | EventMsg::ReasoningContentDelta(_)
        | EventMsg::ReasoningRawContentDelta(_)
        | EventMsg::AgentReasoningSectionBreak(_)) => {
            let notification = item_event_to_server_notification(
                msg,
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        EventMsg::ContextCompacted(..) => {
            // Core still fans out this deprecated event for raw-event and rollout compatibility
            // consumers;
            // v2 clients receive the canonical ContextCompaction item instead.
        }
        EventMsg::DeprecationNotice(event) => {
            let notification = DeprecationNoticeNotification {
                summary: event.summary,
                details: event.details,
            };
            outgoing
                .send_server_notification(ServerNotification::DeprecationNotice(notification))
                .await;
        }
        EventMsg::TokenCount(token_count_event) => {
            handle_token_count_event(conversation_id, event_turn_id, token_count_event, &outgoing)
                .await;
        }
        EventMsg::Error(ev) => {
            let message = ev.message.clone();
            let codex_error_info = ev.codex_error_info.clone();
            // If this error belongs to an in-flight `thread/rollback` request, fail that request
            // (and clear pending state) so subsequent rollbacks are unblocked.
            //
            // Don't send a notification for this error.
            if matches!(
                codex_error_info,
                Some(CoreCodexErrorInfo::ThreadRollbackFailed)
            ) {
                return handle_thread_rollback_failed(
                    conversation_id,
                    message,
                    &thread_state,
                    &outgoing,
                )
                .await;
            };

            if !ev.affects_turn_status() {
                return;
            }
            if thread_state
                .lock()
                .await
                .is_current_summary_turn(&event_turn_id)
            {
                thread_watch_manager
                    .note_system_error(&conversation_id.to_string())
                    .await;
            }

            let turn_error = TurnError {
                message: ev.message,
                codex_error_info: ev.codex_error_info.map(V2CodexErrorInfo::from),
                additional_details: None,
            };
            handle_error_notification(
                conversation_id,
                &event_turn_id,
                turn_error,
                &outgoing,
                &thread_state,
            )
            .await;
        }
        EventMsg::StreamError(ev) => {
            // We don't need to update the turn summary store for stream errors as they are intermediate error states for retries,
            // but we notify the client.
            let turn_error = TurnError {
                message: ev.message,
                codex_error_info: ev.codex_error_info.map(V2CodexErrorInfo::from),
                additional_details: ev.additional_details,
            };
            outgoing
                .send_server_notification(ServerNotification::Error(ErrorNotification {
                    error: turn_error,
                    will_retry: true,
                    thread_id: conversation_id.to_string(),
                    turn_id: event_turn_id.clone(),
                }))
                .await;
        }
        // The legacy ImageView projection duplicates the canonical item events
        // forwarded below as item/started and item/completed notifications.
        EventMsg::ViewImageToolCall(_) => {}
        EventMsg::ItemStarted(event) => {
            let should_emit = match &event.item {
                // Approval flows can emit the command start notification before core
                // emits the canonical item. Reuse the same set to suppress that duplicate.
                CoreTurnItem::CommandExecution(item) => thread_state
                    .lock()
                    .await
                    .turn_summary_for_mut(&event.turn_id)
                    .is_none_or(|summary| {
                        summary.command_execution_started.insert(item.id.clone())
                    }),
                _ => true,
            };
            let dynamic_tool_call_params = match &event.item {
                CoreTurnItem::DynamicToolCall(item)
                    if item.namespace.as_deref() == Some("codex.internal")
                        && item.tool == "code_mode_cell" =>
                {
                    None
                }
                CoreTurnItem::DynamicToolCall(item) => Some(DynamicToolCallParams {
                    thread_id: conversation_id.to_string(),
                    turn_id: event.turn_id.clone(),
                    call_id: item.id.clone(),
                    namespace: item.namespace.clone(),
                    tool: item.tool.clone(),
                    arguments: item.arguments.clone(),
                }),
                _ => None,
            };
            if should_emit {
                let notification = item_event_to_server_notification(
                    EventMsg::ItemStarted(event),
                    &conversation_id.to_string(),
                    &event_turn_id,
                );
                outgoing.send_server_notification(notification).await;
            }
            if let Some(params) = dynamic_tool_call_params {
                let turn_id = params.turn_id.clone();
                let call_id = params.call_id.clone();
                let (_pending_request_id, rx) = outgoing
                    .send_request(ServerRequestPayload::DynamicToolCall(params))
                    .await;
                tokio::spawn(async move {
                    crate::dynamic_tools::on_call_response(turn_id, call_id, rx, conversation)
                        .await;
                });
            }
        }
        EventMsg::ItemCompleted(event) => {
            apply_canonical_item_completed_side_effects(
                &thread_manager,
                &thread_watch_manager,
                &thread_state,
                &event.turn_id,
                &event.item,
            )
            .await;
            let notification = item_event_to_server_notification(
                EventMsg::ItemCompleted(event),
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        msg @ (EventMsg::PatchApplyUpdated(_) | EventMsg::TerminalInteraction(_)) => {
            let notification = item_event_to_server_notification(
                msg,
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        EventMsg::HookStarted(event) => {
            let notification = HookStartedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event.turn_id,
                run: event.run.into(),
            };
            outgoing
                .send_server_notification(ServerNotification::HookStarted(notification))
                .await;
        }
        EventMsg::HookCompleted(event) => {
            let notification = HookCompletedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event.turn_id,
                run: event.run.into(),
            };
            outgoing
                .send_server_notification(ServerNotification::HookCompleted(notification))
                .await;
        }
        EventMsg::RawResponseItem(raw_response_item_event) => {
            maybe_emit_raw_response_item_completed(
                conversation_id,
                &event_turn_id,
                raw_response_item_event.item,
                &outgoing,
            )
            .await;
        }
        EventMsg::PatchApplyBegin(_) | EventMsg::PatchApplyEnd(_) => {
            // Core still fans out these deprecated events for raw-event and rollout compatibility
            // consumers;
            // v2 clients receive the canonical FileChange item instead.
        }
        EventMsg::ExecCommandOutputDelta(exec_command_output_delta_event) => {
            let notification = item_event_to_server_notification(
                EventMsg::ExecCommandOutputDelta(exec_command_output_delta_event),
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        // If this is a TurnAborted, reply to any pending interrupt requests.
        EventMsg::TurnAborted(turn_aborted_event) => {
            if thread_state
                .lock()
                .await
                .is_current_summary_turn(&event_turn_id)
            {
                outgoing.abort_pending_server_requests().await;
                thread_watch_manager
                    .note_turn_aborted(&conversation_id.to_string(), &turn_aborted_event.reason)
                    .await;
            }
            respond_to_pending_interrupts(&thread_state, &outgoing, &event_turn_id).await;
            handle_turn_interrupted(
                conversation_id,
                event_turn_id,
                turn_aborted_event,
                &outgoing,
                &thread_state,
            )
            .await;
        }
        EventMsg::ThreadRolledBack(_rollback_event) => {
            let pending = {
                let mut state = thread_state.lock().await;
                state.take_pending_rollback()
            };

            if let Some(request_id) = pending {
                let thread_list_state_guard = match thread_list_state_permit.acquire().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        outgoing
                            .send_error(
                                request_id,
                                internal_error(format!(
                                    "failed to acquire thread list state permit: {err}"
                                )),
                            )
                            .await;
                        return;
                    }
                };
                let fallback_cwd = conversation.config_snapshot().await.cwd().clone();
                let stored_thread = match conversation
                    .read_thread(
                        /*include_archived*/ true, /*include_history*/ true,
                    )
                    .await
                {
                    Ok(stored_thread) => stored_thread,
                    Err(err) => {
                        drop(thread_list_state_guard);
                        outgoing
                            .send_error(
                                request_id.clone(),
                                internal_error(format!(
                                    "failed to read thread {conversation_id} after rollback: {err}"
                                )),
                            )
                            .await;
                        return;
                    }
                };
                let loaded_status = thread_watch_manager
                    .loaded_status_for_thread(&conversation_id.to_string())
                    .await;
                // This immutable view is coherent now. Projection and transport
                // backpressure must not hold up other thread-list consumers.
                drop(thread_list_state_guard);
                let response = match thread_rollback_response_from_stored_thread(
                    stored_thread,
                    conversation.session_configured().session_id.to_string(),
                    fallback_model_provider.as_str(),
                    &fallback_cwd,
                    loaded_status,
                ) {
                    Ok(response) => response,
                    Err(err) => {
                        outgoing
                            .send_error(request_id.clone(), internal_error(err))
                            .await;
                        return;
                    }
                };

                outgoing.send_response(request_id, response).await;
            }
        }
        // Historical rollouts may contain goal events; the removed feature has no live API.
        EventMsg::ThreadGoalUpdated(_) => {}
        EventMsg::ThreadSettingsApplied(thread_settings_event) => {
            let thread_settings =
                thread_settings_from_core_snapshot(thread_settings_event.thread_settings);
            let changed = {
                let mut state = thread_state.lock().await;
                state.note_thread_settings(thread_settings.clone())
            };
            if changed {
                outgoing
                    .send_server_notification(ServerNotification::ThreadSettingsUpdated(
                        ThreadSettingsUpdatedNotification {
                            thread_id: conversation_id.to_string(),
                            thread_settings,
                        },
                    ))
                    .await;
            }
        }
        EventMsg::TurnDiff(turn_diff_event) => {
            handle_turn_diff(conversation_id, &event_turn_id, turn_diff_event, &outgoing).await;
        }
        EventMsg::PlanUpdate(plan_update_event) => {
            handle_turn_plan_update(
                conversation_id,
                &event_turn_id,
                plan_update_event,
                &outgoing,
            )
            .await;
        }
        EventMsg::ShutdownComplete => {
            thread_watch_manager
                .note_thread_shutdown(&conversation_id.to_string())
                .await;
        }

        EventMsg::AgentMessage(_)
        | EventMsg::ImportedTurnClosed(_)
        | EventMsg::UserMessage(_)
        | EventMsg::AgentReasoning(_)
        | EventMsg::AgentReasoningRawContent(_)
        | EventMsg::SessionConfigured(_)
        | EventMsg::WebSearchBegin(_)
        | EventMsg::WebSearchEnd(_)
        | EventMsg::ImageGenerationBegin(_)
        | EventMsg::ImageGenerationEnd(_) => {
            // These legacy/raw events have no direct v2 projection. Their
            // user-visible equivalents arrive through canonical item events or
            // request responses. Keep this list exhaustive so a new EventMsg
            // cannot become silently invisible to app-server clients.
        }
    }
}

async fn handle_turn_diff(
    conversation_id: ThreadId,
    event_turn_id: &str,
    turn_diff_event: TurnDiffEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let notification = TurnDiffUpdatedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: event_turn_id.to_string(),
        diff: turn_diff_event.unified_diff,
    };
    outgoing
        .send_server_notification(ServerNotification::TurnDiffUpdated(notification))
        .await;
}

async fn handle_turn_plan_update(
    conversation_id: ThreadId,
    event_turn_id: &str,
    plan_update_event: UpdatePlanArgs,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    // `update_plan` is a todo/checklist tool; it is not related to plan-mode updates
    let notification = TurnPlanUpdatedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: event_turn_id.to_string(),
        explanation: plan_update_event.explanation,
        plan: plan_update_event
            .plan
            .into_iter()
            .map(TurnPlanStep::from)
            .collect(),
    };
    outgoing
        .send_server_notification(ServerNotification::TurnPlanUpdated(notification))
        .await;
}

struct TurnCompletionMetadata {
    status: TurnStatus,
    error: Option<TurnError>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    timing: Option<TurnTiming>,
    surfaced_result: Option<codex_protocol::protocol::SurfacedToolResult>,
    origin_connection_id: Option<crate::outgoing_message::ConnectionId>,
}

async fn emit_turn_completed_with_status(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_completion_metadata: TurnCompletionMetadata,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let origin_connection_id = turn_completion_metadata.origin_connection_id;
    let turn = Turn {
        id: event_turn_id,
        items: vec![],
        items_view: TurnItemsView::NotLoaded,
        error: turn_completion_metadata.error,
        status: turn_completion_metadata.status,
        started_at: turn_completion_metadata.started_at,
        completed_at: turn_completion_metadata.completed_at,
        duration_ms: turn_completion_metadata.duration_ms,
        timing: turn_completion_metadata.timing,
        surfaced_result: turn_completion_metadata.surfaced_result,
    };
    let notification = TurnCompletedNotification {
        thread_id: conversation_id.to_string(),
        timing: turn.timing.clone(),
        surfaced_result: turn.surfaced_result.clone(),
        turn,
    };
    outgoing
        .send_server_notification_with_receipts(
            ServerNotification::TurnCompleted(notification),
            origin_connection_id,
        )
        .await;
}

async fn apply_canonical_item_completed_side_effects(
    thread_manager: &Arc<ThreadManager>,
    thread_watch_manager: &ThreadWatchManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    turn_id: &str,
    item: &CoreTurnItem,
) {
    match item {
        CoreTurnItem::CommandExecution(item) => {
            if let Some(summary) = thread_state.lock().await.turn_summary_for_mut(turn_id) {
                summary.command_execution_started.remove(&item.id);
            }
        }
        CoreTurnItem::SubAgentActivity(activity)
            if activity.kind == SubAgentActivityKind::Interrupted =>
        {
            remove_missing_thread_watch(
                thread_manager,
                thread_watch_manager,
                activity.agent_thread_id,
            )
            .await;
        }
        CoreTurnItem::CollabAgentToolCall(item) if item.tool == CoreCollabAgentTool::CloseAgent => {
            for thread_id in &item.receiver_thread_ids {
                remove_missing_thread_watch(thread_manager, thread_watch_manager, *thread_id).await;
            }
        }
        _ => {}
    }
}

async fn remove_missing_thread_watch(
    thread_manager: &Arc<ThreadManager>,
    thread_watch_manager: &ThreadWatchManager,
    thread_id: ThreadId,
) {
    if thread_manager.get_thread(thread_id).await.is_err() {
        thread_watch_manager
            .remove_thread(&thread_id.to_string())
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_command_execution_item(
    conversation_id: &ThreadId,
    turn_id: String,
    item_id: String,
    command: String,
    cwd: LegacyAppPathString,
    command_actions: Vec<V2ParsedCommand>,
    source: CommandExecutionSource,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> bool {
    let first_start = {
        let mut state = thread_state.lock().await;
        state
            .turn_summary_for_mut(&turn_id)
            .is_none_or(|summary| summary.command_execution_started.insert(item_id.clone()))
    };
    if first_start {
        let notification = ItemStartedNotification {
            thread_id: conversation_id.to_string(),
            turn_id,
            started_at_ms: now_unix_timestamp_ms(),
            item: ThreadItem::CommandExecution {
                environment: None,
                output_metadata: None,
                stdout: None,
                stderr: None,
                id: item_id,
                command,
                cwd,
                process_id: None,
                parent_call_id: None,
                parent_cell_id: None,
                runtime_tool_call_id: None,
                execution_id: None,
                source,
                status: CommandExecutionStatus::InProgress,
                command_actions,
                aggregated_output: None,
                exit_code: None,
                duration_ms: None,
            },
        };
        outgoing
            .send_server_notification(ServerNotification::ItemStarted(notification))
            .await;
    }
    first_start
}

#[allow(clippy::too_many_arguments)]
async fn complete_command_execution_item(
    conversation_id: &ThreadId,
    turn_id: String,
    item_id: String,
    command: String,
    cwd: LegacyAppPathString,
    process_id: Option<String>,
    source: CommandExecutionSource,
    command_actions: Vec<V2ParsedCommand>,
    status: CommandExecutionStatus,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let should_emit = thread_state
        .lock()
        .await
        .turn_summary_for_mut(&turn_id)
        .is_some_and(|summary| summary.command_execution_started.remove(&item_id));
    if !should_emit {
        return;
    }

    let item = ThreadItem::CommandExecution {
        environment: None,
        output_metadata: None,
        stdout: None,
        stderr: None,
        id: item_id,
        command,
        cwd,
        process_id,
        parent_call_id: None,
        parent_cell_id: None,
        runtime_tool_call_id: None,
        execution_id: None,
        source,
        status,
        command_actions,
        aggregated_output: None,
        exit_code: None,
        duration_ms: None,
    };
    let notification = ItemCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn_id,
        completed_at_ms: now_unix_timestamp_ms(),
        item,
    };
    outgoing
        .send_server_notification(ServerNotification::ItemCompleted(notification))
        .await;
}

async fn maybe_emit_raw_response_item_completed(
    conversation_id: ThreadId,
    turn_id: &str,
    item: codex_protocol::models::ResponseItem,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let notification = RawResponseItemCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        item,
    };
    outgoing
        .send_server_notification(ServerNotification::RawResponseItemCompleted(notification))
        .await;
}

async fn find_and_remove_turn_summary(
    turn_id: &str,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> TurnSummary {
    let mut state = thread_state.lock().await;
    state.take_turn_summary(turn_id)
}

async fn handle_turn_complete(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_complete_event: TurnCompleteEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let turn_summary = find_and_remove_turn_summary(&event_turn_id, thread_state).await;

    let embedded_error = turn_complete_event.error.as_ref().map(|error| TurnError {
        message: error.message.clone(),
        codex_error_info: error.codex_error_info.clone().map(V2CodexErrorInfo::from),
        additional_details: None,
    });

    let (status, error) = match embedded_error.or(turn_summary.last_error) {
        Some(error) => (TurnStatus::Failed, Some(error)),
        None => (TurnStatus::Completed, None),
    };

    emit_turn_completed_with_status(
        conversation_id,
        event_turn_id,
        TurnCompletionMetadata {
            status,
            error,
            started_at: turn_summary.started_at,
            completed_at: turn_complete_event.completed_at,
            duration_ms: turn_complete_event.duration_ms,
            timing: turn_complete_event.timing,
            surfaced_result: turn_complete_event.surfaced_result,
            origin_connection_id: turn_summary.origin_connection_id,
        },
        outgoing,
    )
    .await
}

async fn handle_turn_interrupted(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_aborted_event: TurnAbortedEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let turn_summary = find_and_remove_turn_summary(&event_turn_id, thread_state).await;
    let internal_error = matches!(
        turn_aborted_event.reason,
        codex_protocol::protocol::TurnAbortReason::InternalError
            | codex_protocol::protocol::TurnAbortReason::ProcessLost
    );
    let status = if internal_error {
        TurnStatus::Failed
    } else {
        TurnStatus::Interrupted
    };
    let error = if internal_error {
        turn_summary.last_error
    } else {
        None
    };

    emit_turn_completed_with_status(
        conversation_id,
        event_turn_id,
        TurnCompletionMetadata {
            status,
            error,
            started_at: turn_summary.started_at,
            completed_at: turn_aborted_event.completed_at,
            duration_ms: turn_aborted_event.duration_ms,
            timing: turn_aborted_event.timing,
            surfaced_result: None,
            origin_connection_id: turn_summary.origin_connection_id,
        },
        outgoing,
    )
    .await
}

async fn handle_thread_rollback_failed(
    _conversation_id: ThreadId,
    message: String,
    thread_state: &Arc<Mutex<ThreadState>>,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let pending_rollback = thread_state.lock().await.take_pending_rollback();

    if let Some(request_id) = pending_rollback {
        outgoing
            .send_error(request_id, invalid_request(message))
            .await;
    }
}

fn thread_rollback_response_from_stored_thread(
    stored_thread: codex_thread_store::StoredThread,
    session_id: String,
    fallback_model_provider: &str,
    fallback_cwd: &AbsolutePathBuf,
    loaded_status: ThreadStatus,
) -> std::result::Result<ThreadRollbackResponse, String> {
    let thread_id = stored_thread.thread_id;
    let (mut thread, history) =
        thread_from_stored_thread(stored_thread, fallback_model_provider, fallback_cwd);
    thread.session_id = session_id;
    let Some(history) = history else {
        return Err(format!(
            "thread {thread_id} did not include persisted history after rollback"
        ));
    };
    populate_thread_turns_from_history(&mut thread, &history.items, /*active_turn*/ None)
        .map_err(|error| format!("rollback completed, but its history view is unavailable; do not repeat the rollback: {error}"))?;
    thread.status = loaded_status;
    Ok(ThreadRollbackResponse { thread })
}

async fn respond_to_pending_interrupts(
    thread_state: &Arc<Mutex<ThreadState>>,
    outgoing: &ThreadScopedOutgoingMessageSender,
    turn_id: &str,
) {
    let pending = {
        let mut state = thread_state.lock().await;
        state.take_pending_interrupts(turn_id)
    };

    let deadline = tokio::time::Instant::now() + crate::outgoing_message::RESOURCE_DELIVERY_TIMEOUT;
    for request_id in pending {
        outgoing
            .send_response_before(request_id, TurnInterruptResponse {}, deadline)
            .await;
    }
}

async fn handle_token_count_event(
    conversation_id: ThreadId,
    turn_id: String,
    token_count_event: TokenCountEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let TokenCountEvent { info, rate_limits } = token_count_event;
    if let Some(token_usage) = info.map(ThreadTokenUsage::from) {
        let notification = ThreadTokenUsageUpdatedNotification {
            thread_id: conversation_id.to_string(),
            turn_id,
            token_usage,
        };
        outgoing
            .send_component_notification_if_changed(ServerNotification::ThreadTokenUsageUpdated(
                notification,
            ))
            .await;
    }
    if let Some(rate_limits) = rate_limits {
        outgoing
            .send_component_notification_if_changed(ServerNotification::AccountRateLimitsUpdated(
                AccountRateLimitsUpdatedNotification {
                    rate_limits: rate_limits.into(),
                },
            ))
            .await;
    }
}

async fn handle_error(turn_id: &str, error: TurnError, thread_state: &Arc<Mutex<ThreadState>>) {
    let mut state = thread_state.lock().await;
    if let Some(summary) = state.turn_summary_for_mut(turn_id) {
        summary.last_error = Some(error);
    }
}

async fn handle_error_notification(
    conversation_id: ThreadId,
    event_turn_id: &str,
    error: TurnError,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    handle_error(event_turn_id, error.clone(), thread_state).await;
    outgoing
        .send_server_notification(ServerNotification::Error(ErrorNotification {
            error,
            will_retry: false,
            thread_id: conversation_id.to_string(),
            turn_id: event_turn_id.to_string(),
        }))
        .await;
}

async fn on_request_user_input_response(
    event_turn_id: String,
    call_id: String,
    pending_request_id: Option<RequestId>,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    user_input_guard: ThreadWatchActiveGuard,
) {
    let response = receiver.await;
    if let Some(pending_request_id) = pending_request_id
        && let Err(error) =
            resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await
    {
        error!(
            ?error,
            "failed to resolve server request on thread listener"
        );
    }
    drop(user_input_guard);
    let Some(response) = request_user_input_response_from_client_result(response) else {
        return;
    };

    if let Err(err) = conversation
        .submit(Op::UserInputAnswer {
            id: event_turn_id,
            call_id: Some(call_id),
            response,
        })
        .await
    {
        error!("failed to submit UserInputAnswer: {err}");
    }
}

fn interrupted_request_user_input_response() -> CoreRequestUserInputResponse {
    CoreRequestUserInputResponse {
        disposition: Some(
            codex_protocol::request_user_input::RequestUserInputDisposition::TransportError,
        ),
        answers: HashMap::new(),
        interrupted: true,
    }
}

fn request_user_input_response_from_client_result(
    response: std::result::Result<ClientRequestResult, oneshot::error::RecvError>,
) -> Option<CoreRequestUserInputResponse> {
    match response {
        Ok(Ok(value)) => {
            let response = match serde_json::from_value::<ToolRequestUserInputResponse>(value) {
                Ok(response) => response,
                Err(err) => {
                    error!("failed to deserialize ToolRequestUserInputResponse: {err}");
                    return Some(interrupted_request_user_input_response());
                }
            };
            Some(CoreRequestUserInputResponse {
                disposition: response.disposition,
                interrupted: response.interrupted,
                answers: response
                    .answers
                    .into_iter()
                    .map(|(id, answer)| {
                        (
                            id,
                            CoreRequestUserInputAnswer {
                                answers: answer.answers,
                            },
                        )
                    })
                    .collect(),
            })
        }
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => None,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            Some(interrupted_request_user_input_response())
        }
        Err(err) => {
            error!("request failed: {err:?}");
            Some(interrupted_request_user_input_response())
        }
    }
}

/// Service initialization forms through the same typed client request/response
/// mapping before a thread/listener exists. Dropping the operation retires its
/// outgoing callback; no provisional thread is exposed to turn/start.
pub(crate) async fn with_startup_mcp_elicitation<F: std::future::Future>(
    outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    connection_id: crate::outgoing_message::ConnectionId,
    future: F,
) -> F::Output {
    codex_mcp::StartupElicitationHandler::new(move |thread_id, request| {
        let outgoing = Arc::clone(&outgoing);
        async move {
            let cancel = || codex_rmcp_client::ElicitationResponse {
                action: codex_rmcp_client::ElicitationAction::Cancel,
                content: None,
                meta: None,
            };
            let Ok(request_body) = request.request.try_into() else {
                return cancel();
            };
            let result = outgoing
                .send_request_to_connections_and_wait(
                    Some(&[connection_id]),
                    ServerRequestPayload::McpServerElicitationRequest(
                        McpServerElicitationRequestParams {
                            thread_id: thread_id.to_string(),
                            turn_id: None,
                            server_name: request.server_name,
                            request: request_body,
                        },
                    ),
                    Some(thread_id),
                )
                .await;
            let response = mcp_server_elicitation_response_from_client_result(result);
            let action = match response.action {
                McpServerElicitationAction::Accept => codex_rmcp_client::ElicitationAction::Accept,
                McpServerElicitationAction::Decline => {
                    codex_rmcp_client::ElicitationAction::Decline
                }
                McpServerElicitationAction::Cancel => codex_rmcp_client::ElicitationAction::Cancel,
            };
            let content = matches!(action, codex_rmcp_client::ElicitationAction::Accept)
                .then(|| response.content.unwrap_or_else(|| serde_json::json!({})));
            codex_rmcp_client::ElicitationResponse {
                action,
                content,
                meta: response.meta,
            }
        }
    })
    .scope(future)
    .await
}

async fn on_mcp_server_elicitation_response(
    server_name: String,
    request_id: codex_protocol::mcp::RequestId,
    pending_request_id: Option<RequestId>,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    permission_guard: ThreadWatchActiveGuard,
) {
    let response = receiver.await;
    if let Some(pending_request_id) = pending_request_id
        && let Err(error) =
            resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await
    {
        error!(
            ?error,
            "failed to resolve server request on thread listener"
        );
    }
    drop(permission_guard);
    let response = mcp_server_elicitation_response_from_client_result(response);

    if let Err(err) = conversation
        .submit(Op::ResolveElicitation {
            server_name,
            request_id,
            decision: response.action.to_core(),
            content: response.content,
            meta: response.meta,
        })
        .await
    {
        error!("failed to submit ResolveElicitation: {err}");
    }
}

fn mcp_server_elicitation_response_from_client_result(
    response: std::result::Result<ClientRequestResult, oneshot::error::RecvError>,
) -> McpServerElicitationRequestResponse {
    match response {
        Ok(Ok(value)) => serde_json::from_value::<McpServerElicitationRequestResponse>(value)
            .unwrap_or_else(|err| {
                error!("failed to deserialize McpServerElicitationRequestResponse: {err}");
                McpServerElicitationRequestResponse {
                    action: McpServerElicitationAction::Decline,
                    content: None,
                    meta: None,
                }
            }),
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => {
            McpServerElicitationRequestResponse {
                action: McpServerElicitationAction::Cancel,
                content: None,
                meta: None,
            }
        }
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            McpServerElicitationRequestResponse {
                action: McpServerElicitationAction::Decline,
                content: None,
                meta: None,
            }
        }
        Err(err) => {
            error!("request failed: {err:?}");
            McpServerElicitationRequestResponse {
                action: McpServerElicitationAction::Decline,
                content: None,
                meta: None,
            }
        }
    }
}

async fn on_request_permissions_response(
    pending_response: PendingRequestPermissionsResponse,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
) {
    let PendingRequestPermissionsResponse {
        call_id,
        conversation_id,
        turn_id,
        requested_permissions,
        request_cwd,
        pending_request_id,
        outgoing,
        receiver,
        request_permissions_guard,
    } = pending_response;
    let response = receiver.await;
    if let Some(pending_request_id) = pending_request_id.clone()
        && let Err(error) =
            resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await
    {
        error!(
            ?error,
            "failed to resolve server request on thread listener"
        );
    }
    drop(request_permissions_guard);
    let response = match request_permissions_response_from_client_result(
        requested_permissions,
        response,
        &request_cwd,
    ) {
        Ok(Some(response)) => response,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(%conversation_id, %turn_id, %call_id, %err, "invalid permission grant; denying this request");
            CoreRequestPermissionsResponse {
                permissions: Default::default(),
                scope: CorePermissionGrantScope::Turn,
            }
        }
    };
    if let Some(pending_request_id) = pending_request_id {
        outgoing
            .track_effective_permissions_approval_response(pending_request_id, response.clone());
    }

    if let Err(err) = conversation
        .submit(Op::RequestPermissionsResponse {
            id: call_id,
            response,
        })
        .await
    {
        error!("failed to submit RequestPermissionsResponse: {err}");
    }
}

struct PendingRequestPermissionsResponse {
    call_id: String,
    conversation_id: ThreadId,
    turn_id: String,
    requested_permissions: CoreRequestPermissionProfile,
    request_cwd: PathUri,
    pending_request_id: Option<RequestId>,
    outgoing: ThreadScopedOutgoingMessageSender,
    receiver: oneshot::Receiver<ClientRequestResult>,
    request_permissions_guard: ThreadWatchActiveGuard,
}

fn request_permissions_response_from_client_result(
    requested_permissions: CoreRequestPermissionProfile,
    response: std::result::Result<ClientRequestResult, oneshot::error::RecvError>,
    cwd: &PathUri,
) -> std::io::Result<Option<CoreRequestPermissionsResponse>> {
    let value = match response {
        Ok(Ok(value)) => value,
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return Ok(None),
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            return Ok(Some(CoreRequestPermissionsResponse {
                permissions: Default::default(),
                scope: CorePermissionGrantScope::Turn,
            }));
        }
        Err(err) => {
            error!("request failed: {err:?}");
            return Ok(Some(CoreRequestPermissionsResponse {
                permissions: Default::default(),
                scope: CorePermissionGrantScope::Turn,
            }));
        }
    };

    let response = serde_json::from_value::<PermissionsRequestApprovalResponse>(value)
        .unwrap_or_else(|err| {
            error!("failed to deserialize PermissionsRequestApprovalResponse: {err}");
            PermissionsRequestApprovalResponse {
                permissions: V2GrantedPermissionProfile::default(),
                scope: codex_app_server_protocol::PermissionGrantScope::Turn,
            }
        });
    let granted_permissions = match response.permissions.into_core_with_cwd(cwd) {
        Ok(permissions) => permissions,
        Err(error) => {
            tracing::warn!(%error, "invalid granted filesystem paths; rejecting the entire permission grant");
            return Ok(Some(CoreRequestPermissionsResponse {
                permissions: Default::default(),
                scope: CorePermissionGrantScope::Turn,
            }));
        }
    };
    let permissions = if granted_permissions.is_empty() {
        CoreRequestPermissionProfile::default()
    } else {
        intersect_uri_permission_profiles(requested_permissions.into(), granted_permissions, cwd)
            .into()
    };
    Ok(Some(CoreRequestPermissionsResponse {
        permissions,
        scope: response.scope.to_core(),
    }))
}

fn map_file_change_approval_decision(decision: FileChangeApprovalDecision) -> ReviewDecision {
    match decision {
        FileChangeApprovalDecision::Accept => ReviewDecision::Approved,
        FileChangeApprovalDecision::AcceptForSession => ReviewDecision::ApprovedForSession,
        FileChangeApprovalDecision::Decline => ReviewDecision::Denied,
        FileChangeApprovalDecision::Cancel => ReviewDecision::Abort,
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_file_change_request_approval_response(
    item_id: String,
    pending_request_id: Option<RequestId>,
    receiver: oneshot::Receiver<ClientRequestResult>,
    codex: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    permission_guard: ThreadWatchActiveGuard,
) {
    let response = receiver.await;
    if let Some(pending_request_id) = pending_request_id
        && let Err(error) =
            resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await
    {
        error!(
            ?error,
            "failed to resolve server request on thread listener"
        );
    }
    drop(permission_guard);
    let decision = match response {
        Ok(Ok(value)) => {
            let response = serde_json::from_value::<FileChangeRequestApprovalResponse>(value)
                .unwrap_or_else(|err| {
                    error!("failed to deserialize FileChangeRequestApprovalResponse: {err}");
                    FileChangeRequestApprovalResponse {
                        decision: FileChangeApprovalDecision::Decline,
                    }
                });

            map_file_change_approval_decision(response.decision)
        }
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            ReviewDecision::Denied
        }
        Err(err) => {
            error!("request failed: {err:?}");
            ReviewDecision::Denied
        }
    };

    if let Err(err) = codex
        .submit(Op::PatchApproval {
            id: item_id,
            decision,
        })
        .await
    {
        error!("failed to submit PatchApproval: {err}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_command_execution_request_approval_response(
    event_turn_id: String,
    conversation_id: ThreadId,
    approval_id: Option<String>,
    item_id: String,
    completion_item: Option<CommandExecutionCompletionItem>,
    pending_request_id: Option<RequestId>,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_state: Arc<Mutex<ThreadState>>,
    permission_guard: ThreadWatchActiveGuard,
) {
    let response = receiver.await;
    if let Some(pending_request_id) = pending_request_id
        && let Err(error) =
            resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await
    {
        error!(
            ?error,
            "failed to resolve server request on thread listener"
        );
    }
    drop(permission_guard);
    let (decision, completion_status) = match response {
        Ok(Ok(value)) => {
            let response = serde_json::from_value::<CommandExecutionRequestApprovalResponse>(value)
                .unwrap_or_else(|err| {
                    error!("failed to deserialize CommandExecutionRequestApprovalResponse: {err}");
                    CommandExecutionRequestApprovalResponse {
                        decision: CommandExecutionApprovalDecision::Decline,
                    }
                });

            let decision = response.decision;

            let (decision, completion_status) = match decision {
                CommandExecutionApprovalDecision::Accept => (ReviewDecision::Approved, None),
                CommandExecutionApprovalDecision::AcceptForSession => {
                    (ReviewDecision::ApprovedForSession, None)
                }
                CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
                    execpolicy_amendment,
                } => (
                    ReviewDecision::ApprovedExecpolicyAmendment {
                        proposed_execpolicy_amendment: execpolicy_amendment.into_core(),
                    },
                    None,
                ),
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment,
                } => {
                    let completion_status = match network_policy_amendment.action {
                        V2NetworkPolicyRuleAction::Allow => None,
                        V2NetworkPolicyRuleAction::Deny => Some(CommandExecutionStatus::Declined),
                    };
                    (
                        ReviewDecision::NetworkPolicyAmendment {
                            network_policy_amendment: network_policy_amendment.into_core(),
                        },
                        completion_status,
                    )
                }
                CommandExecutionApprovalDecision::Decline => (
                    ReviewDecision::Denied,
                    Some(CommandExecutionStatus::Declined),
                ),
                CommandExecutionApprovalDecision::Cancel => (
                    ReviewDecision::Abort,
                    Some(CommandExecutionStatus::Declined),
                ),
            };
            (decision, completion_status)
        }
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            (ReviewDecision::Denied, Some(CommandExecutionStatus::Failed))
        }
        Err(err) => {
            error!("request failed: {err:?}");
            (ReviewDecision::Denied, Some(CommandExecutionStatus::Failed))
        }
    };

    let suppress_subcommand_completion_item = {
        // Subcommand approvals carry an approval ID and point at the parent
        // command item.
        if approval_id.is_some() {
            let state = thread_state.lock().await;
            state
                .turn_summary_for(&event_turn_id)
                .is_some_and(|summary| summary.command_execution_started.contains(&item_id))
        } else {
            false
        }
    };

    if let Some(status) = completion_status
        && !suppress_subcommand_completion_item
        && let Some(completion_item) = completion_item
    {
        complete_command_execution_item(
            &conversation_id,
            event_turn_id.clone(),
            item_id.clone(),
            completion_item.command,
            completion_item.cwd,
            /*process_id*/ None,
            CommandExecutionSource::Agent,
            completion_item.command_actions,
            status,
            &outgoing,
            &thread_state,
        )
        .await;
    }

    if let Err(err) = conversation
        .submit(Op::ExecApproval {
            id: approval_id.unwrap_or_else(|| item_id.clone()),
            turn_id: Some(event_turn_id),
            decision,
        })
        .await
    {
        error!("failed to submit ExecApproval: {err}");
    }
}

fn now_unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

