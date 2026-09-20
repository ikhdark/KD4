use std::collections::HashMap;
use std::sync::Arc;

use async_channel::Receiver;
use async_channel::Sender;
use codex_async_utils::OrCancelExt;
use codex_extension_api::LoadedUserInstructions;
use codex_protocol::approvals::ElicitationAction as ProtocolElicitationAction;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
#[cfg(test)]
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::session::Codex;
use crate::session::CodexSpawnArgs;
use crate::session::CodexSpawnOk;
use crate::session::SUBMISSION_CHANNEL_CAPACITY;
use crate::session::emit_subagent_session_started;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::MultiAgentVersion;

#[cfg(test)]
use crate::session::completed_session_loop_termination;

/// Start an interactive sub-Codex thread and return IO channels.
///
/// The returned `events_rx` yields non-approval events emitted by the sub-agent.
/// Approval requests are handled via `parent_session` and are not surfaced.
/// The returned `ops_tx` allows the caller to submit additional `Op`s to the sub-agent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_codex_thread_interactive(
    config: Config,
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
    subagent_source: SubAgentSource,
    initial_history: Option<InitialHistory>,
) -> Result<Codex, CodexErr> {
    let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_ops, rx_ops) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let conversation_history = initial_history.unwrap_or(InitialHistory::New);
    let forked_from_thread_id = conversation_history.forked_from_id();
    let user_instructions = LoadedUserInstructions {
        instructions: parent_session.user_instructions().await,
        warnings: Vec::new(),
    };
    let CodexSpawnOk { codex, .. } = Box::pin(Codex::spawn(CodexSpawnArgs {
        config,
        allow_provider_model_fallback: false,
        user_instructions,
        installation_id: parent_session.installation_id.clone(),
        auth_manager,
        models_manager,
        environment_manager: parent_session
            .services
            .turn_environments
            .environment_manager(),
        skills_service: Arc::clone(&parent_session.services.skills_service),
        plugins_manager: Arc::clone(&parent_session.services.plugins_manager),
        mcp_manager: Arc::clone(&parent_session.services.mcp_manager),
        code_mode_session_provider: parent_session.services.code_mode_service.session_provider(),
        extensions: Arc::clone(&parent_session.services.extensions),
        conversation_history,
        requested_history_mode: None,
        session_source: SessionSource::SubAgent(subagent_source.clone()),
        forked_from_thread_id,
        parent_thread_id: Some(parent_session.thread_id),
        thread_source: Some(ThreadSource::Subagent),
        originator: parent_ctx.originator.clone(),
        agent_control: parent_session.services.agent_control.clone(),
        dynamic_tools: Vec::new(),
        metrics_service_name: None,
        user_shell_override: None,
        inherited_environments: Some(parent_ctx.environments.clone()),
        inherited_exec_policy: Some(Arc::clone(&parent_session.services.exec_policy)),
        parent_rollout_thread_trace: codex_rollout_trace::ThreadTraceContext::disabled(),
        parent_trace: None,
        environment_selections: parent_ctx.environments.to_selections(),
        thread_extension_init: codex_extension_api::ExtensionDataInit::default(),
        supports_openai_form_elicitation: parent_session
            .services
            .supports_openai_form_elicitation
            .load(std::sync::atomic::Ordering::Relaxed),
        analytics_events_client: Some(parent_session.services.analytics_events_client.clone()),
        thread_store: Arc::clone(&parent_session.services.thread_store),
        attestation_provider: parent_session.services.attestation_provider.clone(),
        external_time_provider: Some(Arc::clone(&parent_session.services.time_provider)),
        inherited_multi_agent_version: Some(MultiAgentVersion::Disabled),
    }))
    .or_cancel(&cancel_token)
    .await??;
    let thread_config = codex.thread_config_snapshot().await;
    let client_metadata = parent_session.app_server_client_metadata().await;
    emit_subagent_session_started(
        &parent_session.services.analytics_events_client,
        client_metadata,
        codex.session.session_id(),
        codex.session.thread_id,
        Some(parent_session.thread_id),
        thread_config,
        subagent_source,
    );
    let codex = Arc::new(codex);

    // Keep both proxy directions on one liveness token. Parent cancellation still
    // cascades, while child termination or either forwarder failing tears down the
    // other direction as well.
    let delegate_liveness = cancel_token.child_token();
    cancel_delegate_when_session_loop_terminates(&codex, delegate_liveness.clone());
    let cancel_token_events = delegate_liveness.clone();
    let cancel_token_ops = delegate_liveness;

    // Forward events from the sub-agent to the consumer, filtering approvals and
    // routing them to the parent session for decisions.
    let parent_session_clone = Arc::clone(&parent_session);
    let parent_ctx_clone = Arc::clone(&parent_ctx);
    let codex_for_events = Arc::clone(&codex);

    tokio::spawn(async move {
        forward_events(
            codex_for_events,
            tx_sub,
            parent_session_clone,
            parent_ctx_clone,
            cancel_token_events,
        )
        .await;
    });

    // Forward ops from the caller to the sub-agent.
    let codex_for_ops = Arc::clone(&codex);
    tokio::spawn(async move {
        forward_ops(codex_for_ops, rx_ops, cancel_token_ops).await;
    });

    Ok(Codex {
        tx_sub: tx_ops,
        rx_event: rx_sub,
        agent_status: codex.agent_status.clone(),
        session: Arc::clone(&codex.session),
        session_loop_termination: codex.session_loop_termination.clone(),
    })
}

fn cancel_delegate_when_session_loop_terminates(
    codex: &Codex,
    delegate_liveness: CancellationToken,
) {
    let session_loop_termination = codex.session_loop_termination.clone();
    tokio::spawn(async move {
        session_loop_termination.await;
        delegate_liveness.cancel();
    });
}

/// Convenience wrapper for one-time use with an initial prompt.
///
/// Internally calls the interactive variant, then immediately submits the provided input.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_codex_thread_one_shot(
    config: Config,
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
    input: Vec<UserInput>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
    subagent_source: SubAgentSource,
    final_output_json_schema: Option<Value>,
    initial_history: Option<InitialHistory>,
) -> Result<Codex, CodexErr> {
    PreparedCodexOneShot::start(
        config,
        auth_manager,
        models_manager,
        parent_session,
        parent_ctx,
        cancel_token,
        subagent_source,
        initial_history,
    )
    .await?
    .submit_once(input, final_output_json_schema)
    .await
}

/// A one-shot child whose thread is initialized without submitting a model
/// request. Callers can finish deterministic preflight, then either submit once
/// or shut the child down without ever generating a turn.
pub(crate) struct PreparedCodexOneShot {
    io: Codex,
    child_cancel: CancellationToken,
    submitted: bool,
}

impl PreparedCodexOneShot {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        parent_session: Arc<Session>,
        parent_ctx: Arc<TurnContext>,
        cancel_token: CancellationToken,
        subagent_source: SubAgentSource,
        initial_history: Option<InitialHistory>,
    ) -> Result<Self, CodexErr> {
        let child_cancel = cancel_token.child_token();
        let io = Box::pin(run_codex_thread_interactive(
            config,
            auth_manager,
            models_manager,
            parent_session,
            parent_ctx,
            child_cancel.clone(),
            subagent_source,
            initial_history,
        ))
        .await?;
        Ok(Self {
            io,
            child_cancel,
            submitted: false,
        })
    }

    pub(crate) async fn submit_once(
        mut self,
        input: Vec<UserInput>,
        final_output_json_schema: Option<Value>,
    ) -> Result<Codex, CodexErr> {
        debug_assert!(
            !self.submitted,
            "prepared one-shot submitted more than once"
        );
        self.io
            .submit(Op::UserInput {
                items: input,
                final_output_json_schema,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await?;
        self.submitted = true;

        // Bridge events so we can observe completion and shut down automatically.
        let (tx_bridge, rx_bridge) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let agent_status = self.io.agent_status.clone();
        let session = Arc::clone(&self.io.session);
        let session_loop_termination = self.io.session_loop_termination.clone();
        tokio::spawn(bridge_one_shot_events(
            self.io,
            tx_bridge,
            self.child_cancel,
        ));

        // For one-shot usage, return a closed `tx_sub` so callers cannot submit
        // additional ops after the initial request. Create a channel and drop the
        // receiver to close it immediately.
        let (tx_closed, rx_closed) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        drop(rx_closed);

        Ok(Codex {
            rx_event: rx_bridge,
            tx_sub: tx_closed,
            agent_status,
            session,
            session_loop_termination,
        })
    }
}

async fn bridge_one_shot_events(
    io: Codex,
    tx_bridge: Sender<Event>,
    child_cancel: CancellationToken,
) {
    while let Ok(Ok(event)) = io.next_event().or_cancel(&child_cancel).await {
        let should_shutdown = matches!(
            event.msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        );
        if !matches!(
            tx_bridge.send(event).or_cancel(&child_cancel).await,
            Ok(Ok(()))
        ) || should_shutdown
        {
            break;
        }
    }
    // The interactive event forwarder owns child shutdown. Signal it even if
    // the consumer closes or stops draining the one-shot output; queuing another
    // proxy operation first could itself block teardown. A healthy receiver has
    // already received the terminal event before this cancellation is signaled.
    child_cancel.cancel();
}

async fn forward_events(
    codex: Arc<Codex>,
    tx_sub: Sender<Event>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
) {
    let cancelled = cancel_token.cancelled();
    tokio::pin!(cancelled);

    loop {
        tokio::select! {
            _ = &mut cancelled => {
                shutdown_delegate(&codex).await;
                break;
            }
            event = codex.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(_) => break,
                };
                match event {
                    Event {
                        id: _,
                        msg:
                            EventMsg::TokenCount(_)
                            | EventMsg::SessionConfigured(_)
                            | EventMsg::McpStartupUpdate(_)
                            | EventMsg::McpStartupComplete(_),
                    } => {}
                    Event {
                        id,
                        msg: EventMsg::ExecApprovalRequest(event),
                    } => {
                        // Initiate approval via parent session; do not surface to consumer.
                        handle_exec_approval(
                            &codex,
                            id,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    Event {
                        msg: EventMsg::ApplyPatchApprovalRequest(event),
                        ..
                    } => {
                        handle_patch_approval(
                            &codex,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    Event {
                        msg: EventMsg::RequestPermissions(event),
                        ..
                    } => {
                        handle_request_permissions(
                            &codex,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    Event {
                        id,
                        msg: EventMsg::RequestUserInput(event),
                    } => {
                        handle_request_user_input(
                            &codex,
                            id,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    Event {
                        msg: EventMsg::ElicitationRequest(event),
                        ..
                    } => {
                        handle_elicitation_request(
                            &codex,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    other => {
                        if !forward_event_or_shutdown(&codex, &tx_sub, &cancel_token, other).await
                        {
                            break;
                        }
                    }
                }
            }
        }
    }
    cancel_token.cancel();
}

/// Ask the delegate to stop and drain its events so background sends do not hit a closed channel.
async fn shutdown_delegate(codex: &Codex) {
    if codex.submit(Op::Interrupt).await.is_err() {
        return;
    }
    if codex.submit(Op::Shutdown {}).await.is_err() {
        return;
    }

    let _ = timeout(Duration::from_millis(500), async {
        while let Ok(event) = codex.next_event().await {
            if matches!(
                event.msg,
                EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
            ) {
                break;
            }
        }
    })
    .await;
}

async fn forward_event_or_shutdown(
    codex: &Codex,
    tx_sub: &Sender<Event>,
    cancel_token: &CancellationToken,
    event: Event,
) -> bool {
    match tx_sub.send(event).or_cancel(cancel_token).await {
        Ok(Ok(())) => true,
        _ => {
            shutdown_delegate(codex).await;
            false
        }
    }
}

/// Forward ops from a caller to a sub-agent, respecting cancellation.
async fn forward_ops(
    codex: Arc<Codex>,
    rx_ops: Receiver<crate::session::QueuedSubmission>,
    cancel_token_ops: CancellationToken,
) {
    loop {
        let submission = match rx_ops.recv().or_cancel(&cancel_token_ops).await {
            Ok(Ok(submission)) => submission,
            Ok(Err(_)) | Err(_) => break,
        };
        // Preserve the original acknowledgement through the proxy. Creating a
        // second submission here would acknowledge forwarding instead of admission.
        if codex.tx_sub.send(submission).await.is_err() {
            break;
        }
    }
    cancel_token_ops.cancel();
}

/// Handle an ExecApprovalRequest by consulting the parent session and replying.
async fn handle_exec_approval(
    codex: &Codex,
    turn_id: String,
    parent_session: &Arc<Session>,
    parent_ctx: &Arc<TurnContext>,
    event: ExecApprovalRequestEvent,
    cancel_token: &CancellationToken,
) {
    let approval_id_for_op = event.effective_approval_id();
    let ExecApprovalRequestEvent {
        call_id,
        approval_id,
        environment_id,
        command,
        cwd,
        cwd_uri,
        reason,
        network_approval_context,
        proposed_execpolicy_amendment,
        additional_permissions,
        available_decisions,
        ..
    } = event;
    let decision = await_approval_with_cancel(
        parent_session.request_command_approval(
            parent_ctx,
            call_id,
            approval_id,
            environment_id,
            command,
            cwd_uri.unwrap_or_else(|| PathUri::from_abs_path(&cwd)),
            reason,
            network_approval_context,
            proposed_execpolicy_amendment,
            additional_permissions,
            available_decisions,
        ),
        parent_session,
        &approval_id_for_op,
        cancel_token,
    )
    .await;

    let _ = codex
        .submit(Op::ExecApproval {
            id: approval_id_for_op,
            turn_id: Some(turn_id),
            decision,
        })
        .await;
}

/// Handle an ApplyPatchApprovalRequest by consulting the parent session and replying.
async fn handle_patch_approval(
    codex: &Codex,
    parent_session: &Arc<Session>,
    parent_ctx: &Arc<TurnContext>,
    event: ApplyPatchApprovalRequestEvent,
    cancel_token: &CancellationToken,
) {
    let ApplyPatchApprovalRequestEvent {
        call_id,
        changes,
        reason,
        grant_root,
        ..
    } = event;
    let approval_id = call_id.clone();

    let decision = {
        let decision =
            parent_session.request_patch_approval(parent_ctx, call_id, changes, reason, grant_root);
        await_approval_with_cancel(decision, parent_session, &approval_id, cancel_token).await
    };
    let _ = codex
        .submit(Op::PatchApproval {
            id: approval_id,
            decision,
        })
        .await;
}

async fn handle_request_user_input(
    codex: &Codex,
    id: String,
    parent_session: &Arc<Session>,
    parent_ctx: &Arc<TurnContext>,
    event: RequestUserInputEvent,
    cancel_token: &CancellationToken,
) {
    let args = RequestUserInputArgs {
        questions: event.questions,
        auto_resolution_ms: event.auto_resolution_ms,
    };
    let response_fut =
        parent_session.request_user_input(parent_ctx, parent_ctx.sub_id.clone(), args);
    let response = await_user_input_with_cancel(
        response_fut,
        parent_session,
        &parent_ctx.sub_id,
        cancel_token,
    )
    .await;
    let _ = codex.submit(Op::UserInputAnswer { id, response }).await;
}

fn protocol_elicitation_id(id: &codex_protocol::mcp::RequestId) -> rmcp::model::RequestId {
    match id {
        codex_protocol::mcp::RequestId::String(value) => {
            rmcp::model::NumberOrString::String(Arc::from(value.as_str()))
        }
        codex_protocol::mcp::RequestId::Integer(value) => {
            rmcp::model::NumberOrString::Number(*value)
        }
    }
}

async fn handle_elicitation_request(
    codex: &Codex,
    parent_session: &Arc<Session>,
    parent_ctx: &Arc<TurnContext>,
    event: ElicitationRequestEvent,
    cancel_token: &CancellationToken,
) {
    let rmcp_id = protocol_elicitation_id(&event.id);
    let request = parent_session.request_mcp_server_elicitation(
        parent_ctx,
        event.server_name.clone(),
        rmcp_id.clone(),
        event.request.clone(),
    );
    let response = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let _ = parent_session.resolve_elicitation(
                event.server_name.clone(),
                rmcp_id,
                codex_rmcp_client::ElicitationResponse {
                    action: codex_rmcp_client::ElicitationAction::Cancel,
                    content: None,
                    meta: None,
                },
            ).await;
            None
        },
        outcome = request => outcome.response,
    };
    let (decision, content, meta) = response.map_or(
        (ProtocolElicitationAction::Cancel, None, None),
        |response| {
            let decision = match response.action {
                codex_rmcp_client::ElicitationAction::Accept => ProtocolElicitationAction::Accept,
                codex_rmcp_client::ElicitationAction::Decline => ProtocolElicitationAction::Decline,
                codex_rmcp_client::ElicitationAction::Cancel => ProtocolElicitationAction::Cancel,
            };
            (decision, response.content, response.meta)
        },
    );
    let _ = codex
        .submit(Op::ResolveElicitation {
            server_name: event.server_name,
            request_id: event.id,
            decision,
            content,
            meta,
        })
        .await;
}

async fn handle_request_permissions(
    codex: &Codex,
    parent_session: &Arc<Session>,
    parent_ctx: &Arc<TurnContext>,
    event: RequestPermissionsEvent,
    cancel_token: &CancellationToken,
) {
    let call_id = event.call_id;
    let args = RequestPermissionsArgs {
        environment_id: event.environment_id,
        reason: event.reason,
        permissions: event.permissions,
    };
    let cwd = event.cwd.unwrap_or_else(|| parent_ctx.cwd().clone());
    let response_fut = parent_session.request_permissions_for_cwd(
        parent_ctx,
        call_id.clone(),
        args,
        cwd,
        cancel_token.clone(),
    );
    let response =
        await_request_permissions_with_cancel(response_fut, parent_session, &call_id, cancel_token)
            .await;
    let _ = codex
        .submit(Op::RequestPermissionsResponse {
            id: call_id,
            response,
        })
        .await;
}

async fn await_user_input_with_cancel<F>(
    fut: F,
    parent_session: &Session,
    sub_id: &str,
    cancel_token: &CancellationToken,
) -> RequestUserInputResponse
where
    F: core::future::Future<Output = Option<RequestUserInputResponse>>,
{
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let empty = RequestUserInputResponse {
                answers: HashMap::new(),
                interrupted: true,
            };
            parent_session
                .notify_user_input_response(sub_id, empty.clone())
                .await;
            empty
        }
        response = fut => response.unwrap_or_else(|| RequestUserInputResponse {
            answers: HashMap::new(),
            interrupted: true,
        }),
    }
}

async fn await_request_permissions_with_cancel<F>(
    fut: F,
    parent_session: &Session,
    call_id: &str,
    cancel_token: &CancellationToken,
) -> RequestPermissionsResponse
where
    F: core::future::Future<Output = Option<RequestPermissionsResponse>>,
{
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let empty = RequestPermissionsResponse {
                permissions: Default::default(),
                scope: PermissionGrantScope::Turn,
            };
            parent_session
                .notify_request_permissions_response(call_id, empty.clone())
                .await;
            empty
        }
        response = fut => response.unwrap_or_else(|| RequestPermissionsResponse {
            permissions: Default::default(),
            scope: PermissionGrantScope::Turn,
        }),
    }
}

/// Await an approval decision, aborting on cancellation.
async fn await_approval_with_cancel<F>(
    fut: F,
    parent_session: &Session,
    approval_id: &str,
    cancel_token: &CancellationToken,
) -> codex_protocol::protocol::ReviewDecision
where
    F: core::future::Future<Output = codex_protocol::protocol::ReviewDecision>,
{
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            parent_session
                .notify_approval(approval_id, codex_protocol::protocol::ReviewDecision::Abort)
                .await;
            codex_protocol::protocol::ReviewDecision::Abort
        }
        decision = fut => {
            decision
        }
    }
}

#[cfg(test)]
#[path = "codex_delegate_tests.rs"]
mod tests;
