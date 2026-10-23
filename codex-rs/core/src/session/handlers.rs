use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Submission;
use std::time::Duration;
use tracing::Instrument;
use tracing::info_span;

use crate::session::SteerInputError;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::session::SessionSettingsUpdate;

use crate::config::Config;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use serde_json::Value;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    // Claim cancellation before returning to dispatch, but do not make the
    // control loop an observer of the cancellation-resistant finalizer.
    sess.request_task_interrupt().await;
}

pub async fn clean_background_terminals(sess: &Arc<Session>) {
    sess.close_unified_exec_processes().await;
}

pub async fn user_input_or_turn(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    client_user_message_id: Option<String>,
) {
    user_input_or_turn_inner(sess, sub_id, op, client_user_message_id).await;
}

pub async fn update_thread_settings(
    sess: &Arc<Session>,
    sub_id: String,
    thread_settings: ThreadSettingsOverrides,
) {
    let updates = thread_settings_update(sess, thread_settings).await;
    match sess.update_settings_durable(&updates).await {
        Ok(msg) => {
            let event = Event { id: sub_id, msg };
            sess.send_event_raw_with_persistence(event, /*persist*/ false)
                .await;
        }
        Err(err) => {
            let codex_error_info = match &err {
                super::ThreadSettingsUpdateError::Invalid(_) => CodexErrorInfo::BadRequest,
                super::ThreadSettingsUpdateError::Persistence(_) => {
                    CodexErrorInfo::InternalServerError
                }
            };
            sess.send_event_raw_without_materializing_rollout(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(codex_error_info),
                }),
            })
            .await;
        }
    }
}

async fn thread_settings_update(
    sess: &Session,
    thread_settings: ThreadSettingsOverrides,
) -> SessionSettingsUpdate {
    let ThreadSettingsOverrides {
        environments,
        workspace_roots,
        profile_workspace_roots,
        approval_policy,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        model,
        effort,
        summary,
        service_tier,
        collaboration_mode,
        personality,
    } = thread_settings;
    let collaboration_mode = match collaboration_mode {
        Some(collaboration_mode) => collaboration_mode,
        None => {
            let state = sess.state.lock().await;
            // Model and reasoning effort live in CollaborationMode settings today, so
            // partial thread-settings updates refresh those fields on the active mode.
            state
                .session_configuration
                .collaboration_mode
                .with_updates(model, effort, /*developer_instructions*/ None)
        }
    };
    SessionSettingsUpdate {
        environments,
        workspace_roots,
        profile_workspace_roots,
        approval_policy,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level,
        collaboration_mode: Some(collaboration_mode),
        reasoning_summary: summary,
        service_tier,
        personality,
        ..Default::default()
    }
}

/// Preparation owns the acknowledgement after it leaves the pending map.
/// Cancelling that future must reject the start rather than drop its reply.
struct TurnStartAdmission(Option<tokio::sync::oneshot::Sender<CodexResult<()>>>);

impl TurnStartAdmission {
    fn send(mut self, result: CodexResult<()>) -> Result<(), CodexResult<()>> {
        // Only `send` and `Drop` take the sender, so it is present here.
        match self.0.take() {
            Some(sender) => sender.send(result),
            None => Err(result),
        }
    }
}

impl Drop for TurnStartAdmission {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(Err(CodexErr::TurnAborted));
        }
    }
}

pub(super) async fn user_input_or_turn_inner(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    client_user_message_id: Option<String>,
) {
    let Op::UserInput {
        items,
        final_output_json_schema,
        responsesapi_client_metadata,
        additional_context,
        thread_settings,
    } = op
    else {
        unreachable!();
    };

    // App-server turn/start submissions register a one-shot admission before
    // enqueueing. Holding this guard through task installation makes the
    // active-turn check and the reserved-ID start one atomic admission.
    let Ok(task_start_permit) = sess.task_start_gate.acquire().await else {
        unreachable!("session-owned task-start semaphore is never closed");
    };
    let mut start_only_admission = {
        let mut task_start = sess.task_start_state.lock().await;
        match task_start.pending_start_only_admissions.remove(&sub_id) {
            Some(admission) => Some((task_start_permit, TurnStartAdmission(Some(admission)))),
            None => {
                drop(task_start_permit);
                None
            }
        }
    };
    if start_only_admission.is_some() && sess.active_turn.lock().await.is_some() {
        if let Some((task_start_permit, admission)) = start_only_admission.take() {
            drop(task_start_permit);
            let _ = admission.send(Err(CodexErr::InvalidRequest(
                "a turn is already active".to_string(),
            )));
        }
        return;
    }

    let final_output_json_schema = match final_output_json_schema
        .map(super::turn_context::PreparedOutputSchema::prepare)
        .transpose()
    {
        Ok(schema) => Some(schema),
        Err(message) => {
            let error = CodexErr::InvalidRequest(message);
            if let Some((permit, admission)) = start_only_admission.take() {
                drop(permit);
                let _ = admission.send(Err(error));
            } else {
                sess.send_event_raw_without_materializing_rollout(Event {
                    id: sub_id,
                    msg: EventMsg::Error(error.to_error_event(None)),
                })
                .await;
            }
            return;
        }
    };
    let emit_thread_settings_applied = thread_settings != ThreadSettingsOverrides::default();
    let updates = if emit_thread_settings_applied {
        thread_settings_update(sess, thread_settings).await
    } else {
        SessionSettingsUpdate::default()
    };
    let session_configuration = match sess.apply_turn_settings(&sub_id, &updates).await {
        Ok(session_configuration) => session_configuration,
        Err(err) => {
            // apply_turn_settings already emits the error event.
            if let Some((task_start_permit, admission)) = start_only_admission.take() {
                drop(task_start_permit);
                let _ = admission.send(Err(err));
            }
            return;
        }
    };
    let initial_settings_persisted = match sess
        .persist_thread_settings_snapshot_if_unmaterialized()
        .await
    {
        Ok(persisted) => persisted,
        Err(err) => {
            let message = format!("failed to persist initial thread settings: {err}");
            sess.send_event_raw_without_materializing_rollout(Event {
                id: sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: message.clone(),
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
            })
            .await;
            if let Some((task_start_permit, admission)) = start_only_admission.take() {
                drop(task_start_permit);
                let _ = admission.send(Err(CodexErr::Fatal(message)));
            }
            return;
        }
    };
    if emit_thread_settings_applied {
        sess.send_event_raw_with_persistence(
            Event {
                id: sub_id.clone(),
                msg: sess.thread_settings_applied_event().await,
            },
            !initial_settings_persisted,
        )
        .await;
    }
    if additional_context.contains_key(crate::validation_task::METADATA_KEY)
        && sess.active_turn.lock().await.is_some()
    {
        sess.send_event_raw(Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(
                CodexErr::InvalidRequest(
                    "validation task/grant metadata cannot change an active task".into(),
                )
                .to_error_event(None),
            ),
        })
        .await;
        return;
    }
    let items = if start_only_admission.is_some() {
        items
    } else {
        match sess
            .steer_input(
                items,
                additional_context.clone(),
                /*expected_turn_id*/ None,
                client_user_message_id.clone(),
                responsesapi_client_metadata.clone(),
            )
            .await
        {
            Ok(_) => return,
            Err(SteerInputError::NoActiveTurn(items)) => items,
            Err(err) => {
                sess.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Error(err.to_error_event()),
                })
                .await;
                return;
            }
        }
    };

    if let Some(entry) = additional_context.get(crate::validation_task::METADATA_KEY)
        && let Err(message) = crate::validation_task::accept_metadata(
            sess,
            entry,
            !session_configuration.session_source.is_non_root_agent(),
        )
        .await
    {
        let error = CodexErr::InvalidRequest(message);
        sess.send_event_raw(Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(error.to_error_event(None)),
        })
        .await;
        if let Some((permit, admission)) = start_only_admission.take() {
            drop(permit);
            let _ = admission.send(Err(error));
        }
        return;
    }
    let current_context = sess
        .new_turn_from_configuration(
            sub_id.clone(),
            session_configuration,
            final_output_json_schema,
        )
        .await;
    sess.maybe_emit_model_warnings_for_turn(current_context.as_ref())
        .await;
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        current_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata);
    }
    current_context.session_telemetry.user_prompt(&items);
    sess.refresh_mcp_servers_if_requested(&current_context)
        .await;
    let additional_context = match sess
        .recoverable_additional_context(additional_context)
        .await
    {
        Ok(values) => values,
        Err(error) => {
            sess.send_event_raw(Event {
                id: sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: error.to_string(),
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
            })
            .await;
            if let Some((permit, admission)) = start_only_admission.take() {
                drop(permit);
                let _ = admission.send(Err(error));
            }
            return;
        }
    };
    let mut task_input = vec![TurnInput::AdditionalContext(additional_context)];
    if !items.is_empty() {
        task_input.push(TurnInput::UserInput {
            content: items,
            client_id: client_user_message_id,
        });
    }
    if let Some((task_start_permit, admission)) = start_only_admission.take() {
        let result = sess
            .start_task_with_admission(
                &task_start_permit,
                Arc::clone(&current_context),
                task_input,
                crate::tasks::RegularTask::new(),
            )
            .await;
        drop(task_start_permit);
        let _ = admission.send(result);
    } else {
        sess.spawn_task(
            Arc::clone(&current_context),
            task_input,
            crate::tasks::RegularTask::new(),
        )
        .await;
    }
}

/// Queues an inter-agent message, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
    admission: Option<tokio::sync::oneshot::Sender<CodexResult<()>>>,
) {
    let trigger_turn = communication.trigger_turn;
    let result = sess
        .input_queue
        .enqueue_mailbox_communication(communication)
        .await;
    let accepted = matches!(&result, Ok(true));
    // Acknowledge queue admission before scheduling a turn, which can itself
    // communicate with other agents. Senders must not wait for turn startup.
    if let Some(admission) = admission {
        let _ = admission.send(result.map(|_| ()));
    }
    if !accepted {
        return;
    }
    crate::agent_communication::emit_agent_communication_receive(&sub_id);
    if trigger_turn {
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
    }
}

pub async fn run_user_shell_command(sess: &Arc<Session>, sub_id: String, command: String) {
    let active_command = command.clone();
    let session = Arc::clone(sess);
    if sess
        .spawn_active_turn_auxiliary(move |turn_context, cancellation_token| async move {
            execute_user_shell_command(
                session,
                turn_context,
                active_command,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        })
        .await
    {
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(
        Arc::clone(&turn_context),
        Vec::new(),
        UserShellCommandTask::new(command),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    sub_id: String,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::approvals::ElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
) {
    let action = match decision {
        codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    let content = match action {
        // Preserve the legacy fallback for clients that only send an action.
        ElicitationAction::Accept => Some(content.unwrap_or_else(|| serde_json::json!({}))),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
        _ => None,
    };
    let response = ElicitationResponse {
        action,
        content,
        meta,
    };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name.clone(), request_id.clone(), response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
        // This is a failed reply, not a failure of the active turn. Keep the
        // server error in diagnostics: it can include the user's form content.
        sess.send_event_raw_without_materializing_rollout(Event {
            id: sub_id,
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Failed to deliver your response to MCP server `{server_name}` for elicitation request `{request_id}`. The request may have expired or the server may have disconnected."
                ),
            }),
        })
        .await;
    }
}

/// Propagate a user's exec approval decision to the session.
/// Also optionally applies an execpolicy amendment.
pub async fn exec_approval(
    sess: &Arc<Session>,
    approval_id: String,
    turn_id: Option<String>,
    decision: ReviewDecision,
) {
    let event_turn_id = turn_id.unwrap_or_else(|| approval_id.clone());
    if let ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment,
    } = &decision
    {
        match sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
        {
            Ok(()) => {
                sess.record_execpolicy_amendment_message(
                    &event_turn_id,
                    proposed_execpolicy_amendment,
                )
                .await;
            }
            Err(err) => {
                let message = format!("Failed to apply execpolicy amendment: {err}");
                tracing::warn!("{message}");
                let warning = EventMsg::Warning(WarningEvent { message });
                sess.send_event_raw(Event {
                    id: event_turn_id.clone(),
                    msg: warning,
                })
                .await;
            }
        }
    }
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&approval_id, other).await,
    }
}

pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&id, other).await,
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    call_id: Option<String>,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response_for_request(&id, call_id.as_deref(), response)
        .await;
}

pub async fn request_permissions_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestPermissionsResponse,
) {
    sess.notify_request_permissions_response(&id, response)
        .await;
}

pub async fn dynamic_tool_response(
    sess: &Arc<Session>,
    turn_id: String,
    id: String,
    response: DynamicToolResponse,
) {
    sess.notify_dynamic_tool_response(&turn_id, &id, response)
        .await;
}

pub async fn refresh_mcp_servers(sess: &Arc<Session>, refresh_config: McpServerRefreshConfig) {
    let mut guard = sess.pending_mcp_server_refresh_config.lock().await;
    *guard = Some(Arc::new(refresh_config));
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    sess.reload_user_config_layer().await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

    sess.spawn_task(Arc::clone(&turn_context), Vec::new(), CompactTask)
        .await;
}

pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
    if num_turns == 0 {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "num_turns must be >= 1".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    // Automatic idle work can start outside the submission loop. Keep admission
    // through snapshot loading and transfer it to the cancellation-shielded commit.
    let Ok(task_start_permit) = Arc::clone(&sess.task_start_gate).acquire_owned().await else {
        unreachable!("session-owned task-start semaphore is never closed");
    };
    let has_active_turn = { sess.active_turn.lock().await.is_some() };
    if has_active_turn {
        drop(task_start_permit);
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot rollback while a turn is in progress.".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    let live_thread = match sess.live_thread_for_persistence("rollback thread") {
        Ok(live_thread) => live_thread,
        Err(_) => {
            drop(task_start_permit);
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "thread rollback requires persisted thread history".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };
    if let Err(err) = live_thread.flush().await {
        drop(task_start_permit);
        sess.send_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: format!("failed to flush thread persistence for rollback replay: {err}"),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let stored_history = match live_thread.load_history(/*include_archived*/ false).await {
        Ok(history) => history,
        Err(err) => {
            drop(task_start_permit);
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("failed to load thread history for rollback replay: {err}"),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };

    let rollback_event = ThreadRolledBackEvent { num_turns };
    let rollback_msg = EventMsg::ThreadRolledBack(rollback_event.clone());
    let replay_items = stored_history
        .items
        .into_iter()
        .chain(std::iter::once(RolloutItem::EventMsg(rollback_msg.clone())))
        .collect::<Vec<_>>();
    if let Err(err) = sess
        .commit_rollback_history(
            &turn_context,
            replay_items,
            rollback_msg.clone(),
            task_start_permit,
        )
        .await
    {
        sess.send_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: format!(
                    "failed to commit rollback marker; no rollback success was published: {err}"
                ),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }
    sess.deliver_event_raw(Event {
        id: turn_context.sub_id.clone(),
        msg: rollback_msg,
    })
    .await;
}

pub(super) const TOOL_HISTORY_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

async fn cancel_startup_work(sess: &Arc<Session>) {
    sess.startup_cancellation.cancel();
    if let Some(startup_prewarm) = sess.take_session_startup_prewarm().await {
        startup_prewarm.abort().await;
    }
    if let Some(startup_transport) = sess.take_session_startup_transport().await {
        startup_transport.abort().await;
    }
}

pub(super) async fn shutdown_unpublished_session(sess: &Arc<Session>) {
    cancel_startup_work(sess).await;
    sess.services.turn_environments.shutdown();
    // Independent services drain concurrently: a blocked snapshot write must
    // not keep a speculative socket, code-mode lease or MCP client alive.
    let (_, _, code_mode, _) = tokio::join!(
        sess.services.turn_environments.finish_shutdown(),
        sess.services.unified_exec_manager.terminate_all_processes(),
        sess.services.code_mode_service.shutdown(),
        sess.services.shutdown_mcp_managers(),
    );
    if let Err(error) = code_mode {
        warn!(%error, "failed to close unpublished code-mode session");
    }
}

/// Runtime teardown result. A timed-out history flush leaves its stalled write
/// owned; persistence retirement queues behind that same write.
struct RuntimeShutdown {
    error: Option<CodexErr>,
    tool_history_flush_timed_out: bool,
}

async fn shutdown_session_runtime(sess: &Arc<Session>) -> RuntimeShutdown {
    let terminal_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let admission_incomplete = tokio::time::timeout_at(terminal_deadline, sess.begin_shutdown())
        .await
        .is_err();
    cancel_startup_work(sess).await;
    // The abort waiter and finalizer keep their own session ownership when the
    // shutdown observation expires. Accepted writes must not be cancelled.
    let abort = sess.terminal_tasks.spawn({
        let sess = Arc::clone(sess);
        async move { sess.abort_all_tasks(TurnAbortReason::SessionShutdown).await }
    });
    let abort_incomplete = !matches!(
        tokio::time::timeout_at(terminal_deadline, abort).await,
        Ok(Ok(()))
    );
    sess.services.turn_environments.shutdown();
    sess.terminal_tasks.close();
    let terminal_incomplete = tokio::time::timeout_at(terminal_deadline, async {
        sess.terminal_tasks.wait().await;
        sess.wait_for_ordered_history_commits().await;
    })
    .await
    .is_err();
    if abort_incomplete || terminal_incomplete {
        warn!("timed out waiting for turn terminal finalization during session shutdown");
    }
    // Match unpublished cleanup only after the admission/producer barrier has
    // actually drained. On uncertain finalization retain the conservative order.
    let (snapshot, code_mode) = if admission_incomplete || abort_incomplete || terminal_incomplete {
        let snapshot = tokio::time::timeout_at(
            terminal_deadline,
            sess.services.turn_environments.finish_shutdown(),
        )
        .await;
        sess.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
        let code_mode = sess.services.code_mode_service.shutdown().await;
        sess.services.shutdown_mcp_managers().await;
        (snapshot, code_mode)
    } else {
        let (snapshot, _, code_mode, _) = tokio::join!(
            tokio::time::timeout_at(
                terminal_deadline,
                sess.services.turn_environments.finish_shutdown()
            ),
            sess.services.unified_exec_manager.terminate_all_processes(),
            sess.services.code_mode_service.shutdown(),
            sess.services.shutdown_mcp_managers(),
        );
        (snapshot, code_mode)
    };
    if snapshot.is_err() {
        warn!("timed out waiting for remote snapshot cleanup during session shutdown");
    }
    if let Err(err) = code_mode {
        warn!("failed to shutdown code mode session: {err}");
    }
    // Every tool-producing owner above is joined before the final checkpoint.
    let tool_history_flush = tokio::time::timeout(
        TOOL_HISTORY_SHUTDOWN_FLUSH_TIMEOUT,
        sess.flush_tool_history_persistence(),
    )
    .await;
    let tool_history_flush_timed_out = tool_history_flush.is_err();
    let persistence_error = match tool_history_flush {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some(CodexErr::Fatal(
            "timed out flushing completed-tool history during shutdown; durability remains pending"
                .to_string(),
        )),
    };
    let error = persistence_error.or_else(|| {
        (admission_incomplete || abort_incomplete || terminal_incomplete).then(|| CodexErr::Fatal(
            "turn finalization did not complete before the shutdown deadline; accepted history writes remain owned, but durability is unconfirmed".to_string(),
        ))
    });
    RuntimeShutdown {
        error,
        tool_history_flush_timed_out,
    }
}

async fn shutdown_thread_persistence(
    sess: &Arc<Session>,
    tool_history_flush_timed_out: bool,
) -> Result<(), String> {
    let live_thread = sess.live_thread().cloned();
    // Never close the writer underneath accepted commits. This owner must be
    // outside terminal_tasks: it waits for that tracker itself to drain.
    let owner = sess.persistence_tasks.spawn({
        let sess = Arc::clone(sess);
        async move {
            sess.terminal_tasks.wait().await;
            sess.wait_for_ordered_history_commits().await;
            // History retires first: its snapshots may still need the thread
            // writer. A failed history checkpoint does not skip normal thread
            // storage cleanup; it only keeps storage ownership below.
            let tool_history = sess
                .tool_history_persistence
                .shutdown()
                .await
                .map_err(|error| format!("completed-tool history checkpoint failed: {error}"));
            let thread = match live_thread {
                Some(live_thread) => live_thread
                    .shutdown()
                    .await
                    .map_err(|error| error.to_string()),
                None => Ok(()),
            };
            let result = match (tool_history, thread) {
                (Ok(()), Ok(())) => {
                    // Stopped thread handles may remain alive for stale-incarnation
                    // checks. They must not retain exclusive storage ownership once
                    // both writers have retired. Failed/pending shutdown keeps it.
                    sess._storage_ownership
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    Ok(())
                }
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(history), Err(thread)) => Err(format!("{history}; {thread}")),
            };
            if let Err(error) = &result {
                warn!(%error, "owned thread persistence shutdown failed");
            }
            *sess
                .persistence_retirement_result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.clone());
            result
        }
    });
    sess.persistence_tasks.close();
    // Retirement queues behind a history flush that already timed out; waiting
    // on that same stalled writer again only delays the pending report. The
    // owner keeps running either way and drain waiters still observe it.
    let observation = if tool_history_flush_timed_out {
        Duration::ZERO
    } else {
        TOOL_HISTORY_SHUTDOWN_FLUSH_TIMEOUT
    };
    match tokio::time::timeout(observation, owner).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(
            "thread persistence shutdown remains owned and pending; durability is unconfirmed"
                .to_string(),
        ),
    }
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    let runtime = shutdown_session_runtime(sess).await;
    finish_shutdown(sess, sub_id, runtime).await
}

async fn finish_shutdown(sess: &Arc<Session>, sub_id: String, runtime: RuntimeShutdown) -> bool {
    let RuntimeShutdown {
        error: runtime_error,
        tool_history_flush_timed_out,
    } = runtime;
    let mut mandatory_error = runtime_error.as_ref().map(ToString::to_string);
    if let Some(error) = runtime_error {
        sess.send_event_raw(Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(error.to_error_event(/*message_prefix*/ None)),
        })
        .await;
    }
    info!("Shutting down Codex instance");
    let turn_count = {
        let state = sess.state.lock().await;
        state
            .history
            .raw_items()
            .iter()
            .filter(|item| is_user_turn_boundary(item))
            .count()
    };
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    sess.emit_thread_stop_lifecycle().await;

    // Gracefully flush and shutdown thread persistence on session end so tests
    // that inspect durable state do not race with the background writer.
    if let Err(e) = shutdown_thread_persistence(sess, tool_history_flush_timed_out).await {
        mandatory_error = Some(match mandatory_error {
            Some(prior) => format!("{prior}; {e}"),
            None => e.clone(),
        });
        warn!("failed to shutdown thread persistence: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: format!("Failed to confirm thread persistence shutdown: {e}"),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }

    *sess
        .persistence_shutdown_result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(mandatory_error.map_or(Ok(()), Err));
    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    if sess.services.rollout_thread_trace.is_enabled() {
        let trace = sess.services.rollout_thread_trace.clone();
        let trace_msg = event.msg.clone();
        // Complete the diagnostic bundle before acknowledging shutdown to consumers.
        let _ = crate::tools::tool_dispatch_trace::run_trace_recording(
            &sess.terminal_tasks,
            move || {
                trace.record_protocol_event(&trace_msg);
                trace.record_ended(codex_rollout_trace::RolloutStatus::Completed);
            },
        )
        .await;
    }
    sess.deliver_event_raw(event).await;
    true
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
    sess.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;
    sess.refresh_mcp_servers_if_requested(&turn_context).await;
    let cwd = turn_context.cwd().clone();
    // Base-branch resolution runs Git and reads repository metadata. The worker owns
    // only its inputs, so dropping this handler cannot start a review after cancellation.
    let resolved =
        tokio::task::spawn_blocking(move || resolve_review_request(review_request, &cwd))
            .await
            .map_err(|error| anyhow::anyhow!("review request resolution task failed: {error}"))
            .and_then(|result| result);
    match resolved {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<super::QueuedSubmission>,
) {
    let _execution_permit_thread_cleanup = sess
        .services
        .agent_control
        .execution_permit_thread_cleanup(sess.thread_id);
    // A single owned future preserves settings/start order. Control messages
    // can bypass slow preparation; dropping that future prevents cancelled
    // preparation from installing a task later. Ordinary admission permits bound
    // the deferred queue, so receiving controls never stops when it fills.
    let mut ordered = std::collections::VecDeque::new();
    let mut pending: Option<(String, bool, futures::future::BoxFuture<'static, bool>)> = None;
    let mut shutdown_received = false;
    let mut shutdown_runtime: Option<(
        String,
        futures::future::BoxFuture<'static, RuntimeShutdown>,
    )> = None;
    loop {
        if let Some((sub_id, runtime)) = shutdown_runtime.as_mut() {
            // Keep completion controls available while execution drains. Once
            // producers stop, close admission and terminalize the entire accepted
            // buffer before retiring persistence or publishing ShutdownComplete.
            tokio::select! {
                biased;
                runtime_shutdown = runtime => {
                    let sub_id = sub_id.clone();
                    rx_sub.close();
                    while let Ok(queued) = rx_sub.try_recv() {
                        let cancellable = matches!(queued.submission.op, Op::UserInput { .. } | Op::Review { .. });
                        cancel_queued_preparation(&sess, queued.submission.id, cancellable, TurnAbortReason::SessionShutdown).await;
                    }
                    finish_shutdown(&sess, sub_id, runtime_shutdown).await;
                    shutdown_received = true;
                    break;
                }
                received = rx_sub.recv(), if !rx_sub.is_closed() => {
                    if let Ok(queued) = received {
                        if matches!(queued.submission.op,
                            Op::ExecApproval { .. } | Op::PatchApproval { .. }
                            | Op::UserInputAnswer { .. } | Op::RequestPermissionsResponse { .. }
                            | Op::DynamicToolResponse { .. } | Op::ResolveElicitation { .. })
                        {
                            dispatch_submission(Arc::clone(&sess), Arc::clone(&config), queued).await;
                        } else {
                            let cancellable = matches!(queued.submission.op, Op::UserInput { .. } | Op::Review { .. });
                            cancel_queued_preparation(&sess, queued.submission.id, cancellable, TurnAbortReason::SessionShutdown).await;
                        }
                    }
                }
            }
            continue;
        }
        if pending.is_none()
            && let Some(queued) = ordered.pop_front()
        {
            let queued: super::QueuedSubmission = queued;
            let cancellable = matches!(
                queued.submission.op,
                Op::UserInput { .. } | Op::Review { .. }
            );
            pending = Some((
                queued.submission.id.clone(),
                cancellable,
                Box::pin(dispatch_submission(
                    Arc::clone(&sess),
                    Arc::clone(&config),
                    queued,
                )),
            ));
        }
        let queued = if let Some((_, _, preparation)) = pending.as_mut() {
            tokio::select! {
                biased;
                should_exit = preparation => {
                    pending = None;
                    if should_exit { shutdown_received = true; break; }
                    continue;
                }
                received = rx_sub.recv() => received,
            }
        } else {
            rx_sub.recv().await
        };
        let Ok(queued) = queued else {
            break;
        };
        let control = super::is_submission_control(&queued.submission.op);
        if !control {
            ordered.push_back(queued);
            continue;
        }
        if matches!(queued.submission.op, Op::Interrupt | Op::Shutdown) {
            let shutting_down = matches!(queued.submission.op, Op::Shutdown);
            // The session flag is not set until the runtime shutdown below runs.
            let abort_reason = if shutting_down {
                TurnAbortReason::SessionShutdown
            } else {
                TurnAbortReason::Interrupted
            };
            if (shutting_down
                || pending
                    .as_ref()
                    .is_some_and(|(_, cancellable, _)| *cancellable))
                && let Some((id, cancellable, preparation)) = pending.take()
            {
                drop(preparation);
                cancel_queued_preparation(&sess, id, cancellable, abort_reason.clone()).await;
            }
            let mut retained = std::collections::VecDeque::new();
            while let Some(deferred) = ordered.pop_front() {
                let cancellable = matches!(
                    deferred.submission.op,
                    Op::UserInput { .. } | Op::Review { .. }
                );
                if shutting_down || cancellable {
                    cancel_queued_preparation(
                        &sess,
                        deferred.submission.id,
                        cancellable,
                        abort_reason.clone(),
                    )
                    .await;
                } else {
                    retained.push_back(deferred);
                }
            }
            ordered = retained;
        }
        if matches!(queued.submission.op, Op::Shutdown) {
            let session = Arc::clone(&sess);
            shutdown_runtime = Some((
                queued.submission.id,
                Box::pin(async move { shutdown_session_runtime(&session).await }),
            ));
            continue;
        }
        if dispatch_submission(Arc::clone(&sess), Arc::clone(&config), queued).await {
            shutdown_received = true;
            break;
        }
    }
    // Closing does not drop async-channel's buffered values. Drain them so
    // outstanding acknowledgements and admission permits cannot remain pinned
    // by a caller retaining its sender after shutdown.
    rx_sub.close();
    while let Ok(queued) = rx_sub.try_recv() {
        ordered.push_back(queued);
    }
    // Teardown owns preparation as well as installed tasks, including EOF. The
    // loop exits only for teardown, so these cancellations are shutdowns.
    if let Some((id, cancellable, preparation)) = pending.take() {
        drop(preparation);
        cancel_queued_preparation(&sess, id, cancellable, TurnAbortReason::SessionShutdown).await;
    }
    for queued in ordered {
        let cancellable = matches!(
            queued.submission.op,
            Op::UserInput { .. } | Op::Review { .. }
        );
        cancel_queued_preparation(
            &sess,
            queued.submission.id,
            cancellable,
            TurnAbortReason::SessionShutdown,
        )
        .await;
    }
    // If the submission loop exits because the channel closed without an
    // explicit shutdown op, still run session teardown.
    if !shutdown_received {
        let RuntimeShutdown {
            error: runtime_error,
            tool_history_flush_timed_out,
        } = shutdown_session_runtime(&sess).await;
        let mut mandatory_error = runtime_error.as_ref().map(ToString::to_string);
        if let Some(error) = runtime_error {
            warn!(
                "completed-tool history was not durable when the submission channel closed: {error}"
            );
        }
        sess.emit_thread_stop_lifecycle().await;
        if let Err(err) = shutdown_thread_persistence(&sess, tool_history_flush_timed_out).await {
            warn!("failed to shutdown thread persistence after submission channel closed: {err}");
            mandatory_error = Some(match mandatory_error {
                Some(prior) => format!("{prior}; {err}"),
                None => err,
            });
        }
        *sess
            .persistence_shutdown_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(mandatory_error.map_or(Ok(()), Err));
    }
    debug!("Agent loop exited");
}

async fn cancel_queued_preparation(
    sess: &Session,
    id: String,
    emit_aborted: bool,
    reason: TurnAbortReason,
) {
    let _permit_cleanup = sess
        .services
        .agent_control
        .execution_permit_cleanup(sess.thread_id, &id);
    if let Some(admission) = sess
        .task_start_state
        .lock()
        .await
        .pending_start_only_admissions
        .remove(&id)
    {
        let _ = admission.send(Err(CodexErr::TurnAborted));
    }
    if emit_aborted {
        sess.send_event_raw(Event {
            id: id.clone(),
            msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(id),
                reason,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        })
        .await;
    }
}

async fn dispatch_submission(
    sess: Arc<Session>,
    config: Arc<Config>,
    queued: super::QueuedSubmission,
) -> bool {
    let super::QueuedSubmission {
        submission: sub,
        mailbox_admission,
        preparation_permit: _preparation_permit,
    } = queued;
    let _execution_permit_cleanup = sess
        .services
        .agent_control
        .execution_permit_cleanup(sess.thread_id, &sub.id);
    debug!(?sub, "Submission");
    let dispatch_span = submission_dispatch_span(&sub);
    async {
        if matches!(&sub.op, Op::UserInput { .. } | Op::Review { .. }) {
            // An interrupt cancels startup for the previous preparation. Only a
            // newly admitted request may reopen it; retries inside cancelled
            // work must continue to observe cancellation. The ordered dispatcher
            // drops that preparation before admitting this one.
            let mut token = sess.services.mcp_startup_cancellation_token.lock().await;
            if token.is_cancelled() {
                *token = tokio_util::sync::CancellationToken::new();
            }
        }
        match sub.op {
            Op::Interrupt => {
                interrupt(&sess).await;
                false
            }
            Op::CleanBackgroundTerminals => {
                clean_background_terminals(&sess).await;
                false
            }
            op @ Op::UserInput { .. } => {
                user_input_or_turn(&sess, sub.id.clone(), op, sub.client_user_message_id).await;
                false
            }
            Op::ThreadSettings { thread_settings } => {
                update_thread_settings(&sess, sub.id.clone(), thread_settings).await;
                false
            }
            Op::InterAgentCommunication { communication } => {
                inter_agent_communication(&sess, sub.id.clone(), communication, mailbox_admission)
                    .await;
                false
            }
            Op::ExecApproval {
                id: approval_id,
                turn_id,
                decision,
            } => {
                exec_approval(&sess, approval_id, turn_id, decision).await;
                false
            }
            Op::PatchApproval { id, decision } => {
                patch_approval(&sess, id, decision).await;
                false
            }
            Op::UserInputAnswer {
                id,
                call_id,
                response,
            } => {
                request_user_input_response(&sess, id, call_id, response).await;
                false
            }
            Op::RequestPermissionsResponse { id, response } => {
                request_permissions_response(&sess, id, response).await;
                false
            }
            Op::DynamicToolResponse {
                turn_id,
                id,
                response,
            } => {
                dynamic_tool_response(&sess, turn_id, id, response).await;
                false
            }
            Op::RefreshMcpServers { config } => {
                refresh_mcp_servers(&sess, config).await;
                false
            }
            Op::ReloadUserConfig => {
                reload_user_config(&sess).await;
                false
            }
            Op::Compact => {
                compact(&sess, sub.id.clone()).await;
                false
            }
            Op::ThreadRollback { num_turns } => {
                thread_rollback(&sess, sub.id.clone(), num_turns).await;
                false
            }
            Op::RunUserShellCommand { command } => {
                run_user_shell_command(&sess, sub.id.clone(), command).await;
                false
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
                content,
                meta,
            } => {
                resolve_elicitation(
                    &sess,
                    sub.id.clone(),
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                )
                .await;
                false
            }
            Op::Shutdown => shutdown(&sess, sub.id.clone()).await,
            Op::Review { review_request } => {
                review(&sess, &config, sub.id.clone(), review_request).await;
                false
            }

            _ => false, // Ignore unknown ops; enum is non_exhaustive to allow extensions.
        }
    }
    .instrument(dispatch_span)
    .await
}

pub(super) fn submission_dispatch_span(sub: &Submission) -> tracing::Span {
    let op_name = sub.op.kind();
    let span_name = format!("op.dispatch.{op_name}");
    let dispatch_span = info_span!(
        "submission_dispatch",
        otel.name = span_name.as_str(),
        submission.id = sub.id.as_str(),
        codex.op = op_name
    );
    if let Some(trace) = sub.trace.as_ref()
        && !set_parent_from_w3c_trace_context(&dispatch_span, trace)
    {
        warn!(
            submission.id = sub.id.as_str(),
            "ignoring invalid submission trace carrier"
        );
    }
    dispatch_span
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use futures::FutureExt;

    fn user_submission(id: &str) -> Submission {
        Submission {
            id: id.to_string(),
            op: Op::UserInput {
                items: Vec::new(),
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: ThreadSettingsOverrides::default(),
            },
            client_user_message_id: None,
            trace: None,
        }
    }

    #[tokio::test]
    async fn interrupted_mcp_preparation_retains_refresh_for_the_next_turn() {
        let (session, context, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let requested = McpServerRefreshConfig {
            mcp_servers: serde_json::json!({"retained-config": {"url": "https://unused.example/mcp", "enabled": false}}),
            mcp_oauth_credentials_store_mode: serde_json::json!("auto"),
            auth_keyring_backend_kind: serde_json::json!("direct"),
        };
        refresh_mcp_servers(&session, requested.clone()).await;
        let old_runtime = session.services.latest_mcp_runtime();
        let pause = Arc::new(super::super::mcp::TestMcpStartupPause::new());
        let (sender, receiver) = async_channel::bounded(8);
        let queued = |submission| super::super::QueuedSubmission {
            submission,
            mailbox_admission: None,
            preparation_permit: None,
        };
        sender
            .send(queued(user_submission("mcp-preparing")))
            .await
            .unwrap();
        let dispatcher = tokio::spawn(super::super::mcp::with_test_mcp_startup_pause(
            Arc::clone(&pause),
            submission_loop(Arc::clone(&session), Arc::clone(&context.config), receiver),
        ));
        tokio::time::timeout(Duration::from_secs(5), pause.started.acquire())
            .await
            .expect("real submission preparation reaches MCP startup")
            .unwrap()
            .forget();
        let mut control = user_submission("interrupt-mcp-preparation");
        control.op = Op::Interrupt;
        sender.send(queued(control)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EventMsg::TurnAborted(event) = events.recv().await.unwrap().msg
                    && event.turn_id.as_deref() == Some("mcp-preparing")
                {
                    break;
                }
            }
        })
        .await
        .expect("interrupt drops the actual preparation owner");
        let retained = session
            .pending_mcp_server_refresh_config
            .lock()
            .await
            .is_some();
        assert!(Arc::ptr_eq(
            &old_runtime,
            &session.services.latest_mcp_runtime()
        ));
        assert!(pause.token.lock().await.as_ref().unwrap().is_cancelled());
        if retained {
            assert!(
                session
                    .mcp_startup_cancellation_token()
                    .await
                    .is_cancelled()
            );
            // A new real admission retries configuration, not the cancelled
            // user's effects. Empty review fails after refresh without sampling.
            let mut retry = user_submission("retry-mcp-config");
            retry.op = Op::Review {
                review_request: ReviewRequest {
                    target: codex_protocol::protocol::ReviewTarget::Custom {
                        instructions: String::new(),
                    },
                    user_facing_hint: None,
                },
            };
            sender.send(queued(retry)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = events.recv().await.unwrap();
                    if event.id == "retry-mcp-config" && matches!(event.msg, EventMsg::Error(_)) {
                        break;
                    }
                }
            })
            .await
            .expect("next admission completes refresh before review rejection");
            let actual =
                codex_mcp::configured_mcp_servers(session.services.latest_mcp_runtime().config());
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(
                    serde_json::from_value::<
                        std::collections::HashMap<String, codex_config::types::McpServerConfig>,
                    >(requested.mcp_servers)
                    .unwrap()
                )
                .unwrap()
            );
            assert!(
                session
                    .pending_mcp_server_refresh_config
                    .lock()
                    .await
                    .is_none()
            );
        }
        let mut control = user_submission("shutdown");
        control.op = Op::Shutdown;
        sender.send(queued(control)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), dispatcher)
            .await
            .unwrap()
            .unwrap();
        assert!(
            retained,
            "interrupt must not silently consume an unapplied configuration refresh"
        );
    }

    #[tokio::test]
    async fn saturated_preparation_admission_still_delivers_controls() {
        let (session, context, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let gate = session.task_start_gate.acquire().await.unwrap();
        // One active preparation and a full deferred channel-sized batch. Hold
        // the remaining slots so this also exercises exhausted admission.
        let reserved = Arc::clone(&session.submission_preparation_slots)
            .acquire_many_owned((super::super::SUBMISSION_CHANNEL_CAPACITY - 9) as u32)
            .await
            .unwrap();
        let (sender, receiver) = async_channel::bounded(8);
        let codex = super::super::Codex {
            tx_sub: sender,
            rx_event: events.clone(),
            agent_status: session.agent_status.subscribe(),
            session: Arc::clone(&session),
            session_loop_termination: futures::future::ready(Ok(())).boxed().shared(),
        };
        codex
            .enqueue_submission(user_submission("blocked"))
            .await
            .unwrap();
        let mut dispatcher = Box::pin(submission_loop(
            Arc::clone(&session),
            Arc::clone(&context.config),
            receiver,
        ));
        assert!(futures::poll!(dispatcher.as_mut()).is_pending());
        for index in 0..8 {
            codex
                .enqueue_submission(user_submission(&format!("deferred-{index}")))
                .await
                .unwrap();
        }
        assert!(futures::poll!(dispatcher.as_mut()).is_pending());
        assert_eq!(session.submission_preparation_slots.available_permits(), 0);
        let mut waiting = Box::pin(codex.enqueue_submission(user_submission("not-admitted")));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(waiting);

        let active = crate::state::ActiveTurn::default();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        active
            .turn_state
            .lock()
            .await
            .try_insert_pending_dynamic_tool("tool".to_string(), context.sub_id.clone(), reply_tx)
            .unwrap();
        *session.active_turn.lock().await = Some(active);
        let response = DynamicToolResponse {
            content_items: Vec::new(),
            success: true,
        };
        codex
            .submit(Op::DynamicToolResponse {
                turn_id: context.sub_id.clone(),
                id: "tool".to_string(),
                response: response.clone(),
            })
            .await
            .unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = reply_rx => result.unwrap(),
                _ = dispatcher.as_mut() => panic!("dispatcher exited"),
            }
        })
        .await
        .expect("a full deferred queue must not block an approval reply");
        assert_eq!(delivered, response);
        *session.active_turn.lock().await = None;
        codex.submit(Op::Interrupt).await.unwrap();
        let aborted = tokio::time::timeout(Duration::from_secs(2), async {
            let mut aborted = Vec::new();
            while aborted.len() < 9 {
                tokio::select! {
                    event = events.recv() => {
                        if let EventMsg::TurnAborted(event) = event.unwrap().msg {
                            aborted.push(event.turn_id.unwrap());
                        }
                    }
                    _ = dispatcher.as_mut() => panic!("dispatcher exited"),
                }
            }
            aborted
        })
        .await
        .expect("interrupt must cancel every admitted preparation");
        let expected = std::iter::once("blocked".to_string())
            .chain((0..8).map(|index| format!("deferred-{index}")))
            .collect::<Vec<_>>();
        assert_eq!(aborted, expected);
        assert_eq!(session.submission_preparation_slots.available_permits(), 9);
        drop(reserved);
        drop(gate);
        assert!(futures::poll!(dispatcher.as_mut()).is_pending());
        assert!(session.active_turn.lock().await.is_none());
        codex.submit(Op::Shutdown).await.unwrap();
        codex
            .enqueue_submission(user_submission("after-shutdown"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), dispatcher)
            .await
            .unwrap();
        let mut terminal_order = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::TurnAborted(aborted)
                    if aborted.turn_id.as_deref() == Some("after-shutdown") =>
                {
                    terminal_order.push("queued-abort");
                }
                EventMsg::ShutdownComplete => terminal_order.push("shutdown-complete"),
                _ => {}
            }
        }
        assert_eq!(terminal_order, vec!["queued-abort", "shutdown-complete"]);
        assert_eq!(
            session.submission_preparation_slots.available_permits(),
            super::super::SUBMISSION_CHANNEL_CAPACITY
        );
    }

    #[tokio::test]
    async fn preparation_admission_releases_cancelled_sends_and_observes_receiver_close() {
        let (session, _, events) = crate::session::tests::make_session_and_context_with_rx().await;
        let reserved = Arc::clone(&session.submission_preparation_slots)
            .acquire_many_owned((super::super::SUBMISSION_CHANNEL_CAPACITY - 1) as u32)
            .await
            .unwrap();
        let (sender, receiver) = async_channel::bounded(1);
        let codex = super::super::Codex {
            tx_sub: sender,
            rx_event: events,
            agent_status: session.agent_status.subscribe(),
            session: Arc::clone(&session),
            session_loop_termination: futures::future::ready(Ok(())).boxed().shared(),
        };
        codex.submit(Op::Interrupt).await.unwrap();
        let mut sending = Box::pin(codex.enqueue_submission(user_submission("cancelled-send")));
        assert!(futures::poll!(sending.as_mut()).is_pending());
        assert_eq!(session.submission_preparation_slots.available_permits(), 0);
        drop(sending);
        assert_eq!(session.submission_preparation_slots.available_permits(), 1);
        drop(receiver.recv().await.unwrap());
        codex
            .enqueue_submission(user_submission("queued"))
            .await
            .unwrap();
        let mut waiting = Box::pin(codex.enqueue_submission(user_submission("waiting")));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        receiver.close();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap(),
            Err(CodexErr::InternalAgentDied)
        ));
        assert!(matches!(
            codex.enqueue_submission(user_submission("closed")).await,
            Err(CodexErr::InternalAgentDied)
        ));
        // Closing retains buffered values; the dispatcher drains them on teardown.
        drop(receiver.recv().await.unwrap());
        assert_eq!(session.submission_preparation_slots.available_permits(), 1);
        drop(reserved);
    }

    #[tokio::test]
    async fn preparation_acknowledgement_reports_cancellation_or_actual_result() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        drop(TurnStartAdmission(Some(sender)));
        assert!(matches!(
            receiver.await.unwrap(),
            Err(CodexErr::TurnAborted)
        ));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        TurnStartAdmission(Some(sender)).send(Ok(())).unwrap();
        assert!(receiver.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn controls_bypass_blocked_startup_and_cancel_its_owned_future() {
        let (session, context, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let permit = session.task_start_gate.acquire().await.unwrap();
        let active = crate::state::ActiveTurn::default();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        active
            .turn_state
            .lock()
            .await
            .try_insert_pending_dynamic_tool("tool".to_string(), context.sub_id.clone(), reply_tx)
            .unwrap();
        *session.active_turn.lock().await = Some(active);
        let (sender, receiver) = async_channel::bounded(8);
        let submit = |id: &str, op| super::super::QueuedSubmission {
            submission: Submission {
                id: id.to_string(),
                op,
                client_user_message_id: None,
                trace: None,
            },
            mailbox_admission: None,
            preparation_permit: None,
        };
        sender
            .send(submit(
                "blocked",
                Op::UserInput {
                    items: Vec::new(),
                    final_output_json_schema: None,
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: ThreadSettingsOverrides::default(),
                },
            ))
            .await
            .unwrap();
        let mut dispatcher = Box::pin(submission_loop(
            Arc::clone(&session),
            Arc::clone(&context.config),
            receiver,
        ));
        assert!(futures::poll!(dispatcher.as_mut()).is_pending());
        let response = DynamicToolResponse {
            content_items: Vec::new(),
            success: true,
        };
        sender
            .send(submit(
                "reply",
                Op::DynamicToolResponse {
                    turn_id: context.sub_id.clone(),
                    id: "tool".to_string(),
                    response: response.clone(),
                },
            ))
            .await
            .unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! { result = reply_rx => result.unwrap(), _ = dispatcher.as_mut() => panic!("dispatcher exited") }
        }).await.expect("reply must bypass held startup gate");
        assert_eq!(delivered, response);
        *session.active_turn.lock().await = None;
        let (admission, rejected) = tokio::sync::oneshot::channel();
        session
            .task_start_state
            .lock()
            .await
            .pending_start_only_admissions
            .insert("deferred".to_string(), admission);
        sender
            .send(submit(
                "deferred",
                Op::UserInput {
                    items: Vec::new(),
                    final_output_json_schema: None,
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: ThreadSettingsOverrides::default(),
                },
            ))
            .await
            .unwrap();
        sender
            .send(submit("interrupt", Op::Interrupt))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut aborted_ids = Vec::new();
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let event = event.unwrap();
                        if let EventMsg::TurnAborted(aborted) = event.msg {
                            aborted_ids.push(aborted.turn_id.unwrap());
                            if aborted_ids.len() == 2 {
                                assert_eq!(aborted_ids, ["blocked", "deferred"]);
                                break;
                            }
                        }
                    }
                    _ = dispatcher.as_mut() => panic!("dispatcher exited"),
                }
            }
        })
        .await
        .expect("interrupt must cancel preparation while the gate is held");
        assert!(matches!(
            rejected.await.unwrap(),
            Err(CodexErr::TurnAborted)
        ));
        assert!(
            session
                .task_start_state
                .lock()
                .await
                .pending_start_only_admissions
                .is_empty()
        );
        drop(permit);
        assert!(futures::poll!(dispatcher.as_mut()).is_pending());
        assert!(
            session.active_turn.lock().await.is_none(),
            "cancelled preparation cannot install a task"
        );
        sender.send(submit("shutdown", Op::Shutdown)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), dispatcher)
            .await
            .unwrap();
    }
}
