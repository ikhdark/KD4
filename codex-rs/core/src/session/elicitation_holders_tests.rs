use std::collections::HashMap;

use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use crate::state::TurnTerminalCoordinator;

async fn wait_until_held(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| *paused)
        .await
        .expect("elicitation service should remain available");
}

async fn wait_until_released(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| !*paused)
        .await
        .expect("elicitation service should remain available");
}

pub(crate) struct InteractionOwnerTask;

// Registry-cleanup controls need a live owner but must not emit startup events
// into their deliberately full one-slot event channel.
pub(crate) fn registry_interaction_owner(
    turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
) -> ActiveTurn {
    let worker = tokio::spawn(async {});
    let cancellation = CancellationToken::new();
    let terminal = TurnTerminalCoordinator::new(turn.sub_id.clone());
    ActiveTurn {
        task: Some(crate::state::RunningTask {
            done: std::sync::Arc::new(tokio::sync::Notify::new()),
            worker_done: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            kind: crate::state::TaskKind::Regular,
            task: std::sync::Arc::new(InteractionOwnerTask),
            auxiliary_cancellation_token: cancellation.child_token(),
            cancellation_token: cancellation,
            auxiliary_tasks: tokio::task::JoinSet::new(),
            worker_abort_handle: worker.abort_handle(),
            task_span: tracing::Span::none(),
            turn_context: turn,
            _agent_execution_guard: None,
        }),
        terminal: Some(terminal),
        ..Default::default()
    }
}

impl crate::tasks::SessionTask for InteractionOwnerTask {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.interaction_owner_fixture"
    }
    fn run(
        self: std::sync::Arc<Self>,
        _session: std::sync::Arc<crate::session::session::Session>,
        _turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation: CancellationToken,
    ) -> futures::future::BoxFuture<'static, crate::tasks::SessionTaskResult> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Ok(crate::tasks::TurnTaskResult::default())
        })
    }
}

#[tokio::test]
async fn mcp_elicitation_rejects_idle_taskless_claimed_and_wrong_owner() {
    use codex_protocol::approvals::ElicitationRequest;
    use rmcp::model::RequestId;
    for owner in [None, Some("taskless"), Some("claimed"), Some("successor")] {
        let mut terminal_permit = None;
        let (session, turn, events) = make_session_and_context_with_rx().await;
        session
            .services
            .latest_mcp_runtime()
            .manager()
            .set_elicitations_auto_deny(false);
        if let Some(owner) = owner {
            if owner == "successor" {
                let successor = session
                    .new_default_turn_with_sub_id("successor".into())
                    .await;
                session
                    .spawn_task(successor, Vec::new(), InteractionOwnerTask)
                    .await;
            } else {
                let terminal = TurnTerminalCoordinator::new(turn.sub_id.clone());
                if owner == "claimed" {
                    terminal_permit = terminal.try_claim();
                }
                *session.active_turn.lock().await = Some(ActiveTurn {
                    terminal: Some(terminal),
                    ..Default::default()
                });
            }
        }
        let result = session
            .request_mcp_server_elicitation(
                &turn,
                "probe".into(),
                RequestId::String("probe".into()),
                ElicitationRequest::Form {
                    meta: None,
                    message: "old request".into(),
                    requested_schema: serde_json::json!({"type":"object"}),
                },
            )
            .await;
        assert!(!result.sent);
        assert!(result.response.is_none());
        assert!(!*session.subscribe_elicitation_pause_state().borrow());
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event.msg,
                codex_protocol::protocol::EventMsg::ElicitationRequest(_)
            ));
        }
        let turn_state = session
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|active| std::sync::Arc::clone(&active.turn_state));
        if let Some(turn_state) = turn_state {
            assert!(
                !turn_state
                    .lock()
                    .await
                    .has_pending_elicitation("probe", &RequestId::String("probe".into()))
            );
        }
        drop(terminal_permit);
        if owner == Some("successor") {
            session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
        } else {
            session.active_turn.lock().await.take();
        }
    }
}

#[tokio::test]
async fn matching_live_mcp_owner_receives_one_question_and_answer() {
    use codex_protocol::approvals::ElicitationRequest;
    use rmcp::model::RequestId;
    let (session, turn, events) = make_session_and_context_with_rx().await;
    session
        .services
        .latest_mcp_runtime()
        .manager()
        .set_elicitations_auto_deny(false);
    session
        .spawn_task(turn.clone(), Vec::new(), InteractionOwnerTask)
        .await;
    let id = RequestId::String("live-question".into());
    let mut request = Box::pin(session.request_mcp_server_elicitation(
        &turn,
        "probe".into(),
        id.clone(),
        ElicitationRequest::Form {
            meta: None,
            message: "current request".into(),
            requested_schema: serde_json::json!({"type":"object"}),
        },
    ));
    assert!(futures::poll!(request.as_mut()).is_pending());
    loop {
        if let codex_protocol::protocol::EventMsg::ElicitationRequest(event) =
            events.recv().await.unwrap().msg
        {
            assert_eq!(event.turn_id.as_deref(), Some(turn.sub_id.as_str()));
            break;
        }
    }
    let response = codex_rmcp_client::ElicitationResponse {
        action: codex_rmcp_client::ElicitationAction::Accept,
        content: Some(serde_json::json!({})),
        meta: None,
    };
    session
        .resolve_elicitation("probe".into(), id, response.clone())
        .await
        .unwrap();
    let result = request.await;
    assert!(result.sent);
    assert_eq!(result.response, Some(response));
    assert!(!*session.subscribe_elicitation_pause_state().borrow());
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test]
async fn command_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();
    let cwd = turn_context.cwd().clone();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd,
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::Approved
    );
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn patch_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_patch_approval(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    HashMap::new(),
                    /*reason*/ None,
                    /*grant_root*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::Approved
    );
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn permission_request_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            let environment = turn_context
                .environments
                .primary()
                .expect("primary environment")
                .selection();
            session
                .request_permissions_for_environment(
                    &turn_context,
                    "call-1".to_string(),
                    RequestPermissionsArgs {
                        environment_id: None,
                        reason: None,
                        permissions: RequestPermissionProfile::default(),
                    },
                    environment,
                    CancellationToken::new(),
                )
                .await
        }
    });

    events.recv().await.expect("permission request event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_request_permissions_response(
            "call-1",
            RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
            },
        )
        .await;
    assert_eq!(
        request.await.expect("permission request task"),
        Some(RequestPermissionsResponse {
            permissions: RequestPermissionProfile::default(),
            scope: PermissionGrantScope::Turn,
        })
    );
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn request_user_input_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let terminal = TurnTerminalCoordinator::new(turn_context.sub_id.clone());
    let active_turn = ActiveTurn {
        terminal: Some(terminal.clone()),
        ..ActiveTurn::default()
    };
    *session.active_turn.lock().await = Some(active_turn);
    let mut pause_state = session.subscribe_elicitation_pause_state();

    session
        .notify_user_input_response(
            "stray-turn",
            RequestUserInputResponse {
                disposition: None,
                answers: HashMap::new(),
                interrupted: true,
            },
        )
        .await;
    assert!(
        !terminal.interrupt_pending(),
        "a stray interruption response must not fence the active turn"
    );

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });

    events.recv().await.expect("request user input event");
    wait_until_held(&mut pause_state).await;

    let response = RequestUserInputResponse {
        disposition: None,
        answers: HashMap::new(),
        interrupted: true,
    };
    session
        .notify_user_input_response(&turn_context.sub_id, response)
        .await;

    let response = request
        .await
        .expect("request user input task")
        .expect("accepted request user input response");
    assert!(response.interrupted);
    assert!(
        terminal.interrupt_pending(),
        "the accepted interrupted response must fence the active turn"
    );
    wait_until_released(&mut pause_state).await;
}
