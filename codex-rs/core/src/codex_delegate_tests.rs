use super::*;
use async_channel::bounded;
use codex_protocol::models::NetworkPermissions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpStartupCompleteEvent;
use codex_protocol::protocol::McpStartupStatus;
use codex_protocol::protocol::McpStartupUpdateEvent;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputEvent;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::timeout;

#[tokio::test]
async fn forward_events_filters_private_events_before_blocked_send_is_cancelled() {
    let (tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let codex = Arc::new(Codex {
        tx_sub,
        rx_event: rx_events,
        agent_status,
        session: Arc::clone(&session),
        session_loop_termination: completed_session_loop_termination(),
    });

    let (tx_out, rx_out) = bounded(1);
    tx_out
        .send(Event {
            id: "full".to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let forward = tokio::spawn(forward_events(
        Arc::clone(&codex),
        tx_out.clone(),
        session,
        ctx,
        cancel.clone(),
    ));

    for msg in [
        EventMsg::McpStartupUpdate(McpStartupUpdateEvent {
            server: "pending".to_string(),
            status: McpStartupStatus::Starting,
        }),
        EventMsg::McpStartupComplete(McpStartupCompleteEvent::default()),
    ] {
        tx_events
            .send(Event {
                id: "delegate-startup".to_string(),
                msg,
            })
            .await
            .unwrap();
    }
    let visible_msg = EventMsg::RawResponseItem(RawResponseItemEvent {
        item: ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    });
    for id in ["visible-1", "visible-2", "blocked"] {
        tx_events
            .send(Event {
                id: id.to_string(),
                msg: visible_msg.clone(),
            })
            .await
            .unwrap();
    }

    drop(tx_events);
    let received = rx_out.recv().await.expect("prefilled event missing");
    assert_eq!(received.id, "full");
    let received = rx_out.recv().await.expect("visible event missing");
    assert_eq!(received.id, "visible-1");
    cancel.cancel();
    timeout(std::time::Duration::from_millis(1000), forward)
        .await
        .expect("forward_events hung")
        .expect("forward_events join error");

    let mut ops = Vec::new();
    while let Ok(sub) = rx_sub.try_recv() {
        ops.push(sub.submission.op);
    }
    assert!(
        ops.iter().any(|op| matches!(op, Op::Interrupt)),
        "expected Interrupt op after cancellation"
    );
    assert!(
        ops.iter().any(|op| matches!(op, Op::Shutdown)),
        "expected Shutdown op after cancellation"
    );
}

#[tokio::test]
async fn forward_ops_preserves_mailbox_admission_acknowledgement() {
    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, _ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let codex = Arc::new(Codex {
        tx_sub,
        rx_event: rx_events,
        agent_status,
        session,
        session_loop_termination: completed_session_loop_termination(),
    });
    let (tx_ops, rx_ops) = bounded(1);
    let forward = tokio::spawn(forward_ops(
        Arc::clone(&codex),
        rx_ops,
        CancellationToken::new(),
    ));
    let (admission_tx, admission_rx) = oneshot::channel();
    tx_ops
        .send(crate::session::QueuedSubmission {
            submission: Submission {
                id: "mailbox-overflow".to_string(),
                op: Op::InterAgentCommunication {
                    communication: codex_protocol::protocol::InterAgentCommunication::new(
                        codex_protocol::AgentPath::root(),
                        codex_protocol::AgentPath::try_from("/root/worker").expect("worker path"),
                        Vec::new(),
                        "message".to_string(),
                        false,
                    ),
                },
                client_user_message_id: None,
                trace: None,
            },
            mailbox_admission: Some(admission_tx),
        })
        .await
        .expect("submit to proxy");
    let forwarded = timeout(Duration::from_secs(5), rx_sub.recv())
        .await
        .expect("forwarding timed out")
        .expect("forwarded submission");
    assert_eq!(forwarded.submission.id, "mailbox-overflow");
    forwarded
        .mailbox_admission
        .expect("original acknowledgement")
        .send(Err(CodexErr::InvalidRequest(
            "session mailbox is full".to_string(),
        )))
        .expect("sender is waiting");
    let error = timeout(Duration::from_secs(5), admission_rx)
        .await
        .expect("acknowledgement timed out")
        .expect("acknowledgement dropped")
        .expect_err("sender must receive admission rejection");
    assert!(
        matches!(error, CodexErr::InvalidRequest(message) if message == "session mailbox is full")
    );
    drop(tx_ops);
    timeout(Duration::from_secs(5), forward)
        .await
        .expect("proxy shutdown timed out")
        .expect("proxy task");
}

struct CancellationObservedTask {
    started: Arc<tokio::sync::Notify>,
    stopped: Arc<tokio::sync::Notify>,
}

impl crate::tasks::SessionTask for CancellationObservedTask {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.delegate_cancel_test"
    }
    fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation: CancellationToken,
    ) -> futures::future::BoxFuture<'static, crate::tasks::SessionTaskResult> {
        Box::pin(async move {
            self.started.notify_one();
            cancellation.cancelled().await;
            self.stopped.notify_one();
            Ok(crate::tasks::TurnTaskResult::default())
        })
    }
}

#[tokio::test]
async fn full_delegate_mailbox_cancels_forwarding_and_the_running_child() {
    let (session, ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::new(tokio::sync::Notify::new());
    session
        .spawn_task(
            ctx,
            Vec::new(),
            CancellationObservedTask {
                started: Arc::clone(&started),
                stopped: Arc::clone(&stopped),
            },
        )
        .await;
    timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("child starts");
    let (tx_sub, rx_sub) = bounded(1);
    let queued = |id: &str| crate::session::QueuedSubmission {
        submission: Submission {
            id: id.to_string(),
            op: Op::Interrupt,
            client_user_message_id: None,
            trace: None,
        },
        mailbox_admission: None,
    };
    tx_sub.send(queued("fills-child-mailbox")).await.unwrap();
    let (_tx_events, rx_event) = bounded(1);
    let (_status_tx, agent_status) = watch::channel(AgentStatus::Running);
    let codex = Arc::new(Codex {
        tx_sub,
        rx_event,
        agent_status,
        session: Arc::clone(&session),
        session_loop_termination: completed_session_loop_termination(),
    });
    let (tx_ops, rx_ops) = bounded(1);
    let (admission_tx, admission_rx) = oneshot::channel();
    let mut blocked = queued("blocked-forward");
    blocked.mailbox_admission = Some(admission_tx);
    tx_ops.send(blocked).await.unwrap();
    let cancel = CancellationToken::new();
    let forward = tokio::spawn(forward_ops(
        Arc::clone(&codex),
        rx_ops.clone(),
        cancel.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        while !rx_ops.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("forwarder takes queued submission");
    cancel.cancel();
    timeout(Duration::from_secs(1), forward)
        .await
        .expect("full queue must not trap cancellation")
        .unwrap();
    assert!(
        admission_rx.await.is_err(),
        "a canceled forwarding operation cannot acknowledge admission"
    );
    let start_gate = session.task_start_gate.acquire().await.unwrap();
    timeout(Duration::from_secs(1), shutdown_delegate(&codex))
        .await
        .expect("shutdown deadline covers the task-start gate");
    assert!(
        session
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(rx_sub.is_closed());
    drop(start_gate);
    timeout(Duration::from_secs(5), stopped.notified())
        .await
        .expect("owned teardown must cancel the actual child after the drain deadline");
}

#[tokio::test]
async fn forward_ops_preserves_submission_trace_context() {
    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, _ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let codex = Arc::new(Codex {
        tx_sub,
        rx_event: rx_events,
        agent_status,
        session,
        session_loop_termination: completed_session_loop_termination(),
    });
    let (tx_ops, rx_ops) = bounded(1);
    let cancel = CancellationToken::new();
    let forward = tokio::spawn(forward_ops(Arc::clone(&codex), rx_ops, cancel));

    let submission = Submission {
        id: "sub-1".to_string(),
        op: Op::Interrupt,
        client_user_message_id: None,
        trace: Some(codex_protocol::protocol::W3cTraceContext {
            traceparent: Some(
                "00-1234567890abcdef1234567890abcdef-1234567890abcdef-01".to_string(),
            ),
            tracestate: Some("vendor=state".to_string()),
        }),
    };
    tx_ops
        .send(crate::session::QueuedSubmission {
            submission: submission.clone(),
            mailbox_admission: None,
        })
        .await
        .unwrap();
    drop(tx_ops);

    let forwarded = timeout(Duration::from_secs(1), rx_sub.recv())
        .await
        .expect("forward_ops hung")
        .expect("forwarded submission missing");
    assert_eq!(submission.id, forwarded.submission.id);
    assert_eq!(submission.op, forwarded.submission.op);
    assert_eq!(submission.trace, forwarded.submission.trace);

    timeout(Duration::from_secs(1), forward)
        .await
        .expect("forward_ops did not exit")
        .expect("forward_ops join error");
}

#[tokio::test]
async fn session_loop_termination_closes_proxy_with_live_child_event_sender() {
    let (tx_child_sub, rx_child_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_child_events, rx_child_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let (termination_tx, termination_rx) = oneshot::channel();
    let session_loop_termination =
        crate::session::session_loop_termination_from_handle(tokio::spawn(async move {
            let _ = termination_rx.await;
        }));
    let child = Arc::new(Codex {
        tx_sub: tx_child_sub,
        rx_event: rx_child_events,
        agent_status,
        session: Arc::clone(&session),
        session_loop_termination: session_loop_termination.clone(),
    });

    let (tx_outer_events, rx_outer_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_outer_ops, rx_outer_ops) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let delegate_liveness = CancellationToken::new();
    cancel_delegate_when_session_loop_terminates(&child, delegate_liveness.clone());
    let events = tokio::spawn(forward_events(
        Arc::clone(&child),
        tx_outer_events,
        Arc::clone(&session),
        ctx,
        delegate_liveness.clone(),
    ));
    let ops = tokio::spawn(forward_ops(
        Arc::clone(&child),
        rx_outer_ops,
        delegate_liveness,
    ));
    let outer = Codex {
        tx_sub: tx_outer_ops,
        rx_event: rx_outer_events,
        agent_status: child.agent_status.clone(),
        session,
        session_loop_termination,
    };

    drop(rx_child_sub);
    termination_tx.send(()).expect("termination receiver alive");
    timeout(Duration::from_secs(1), async {
        events.await.expect("forward_events join error");
        ops.await.expect("forward_ops join error");
    })
    .await
    .expect("delegate proxy did not close after child termination");

    assert!(matches!(
        outer.submit(Op::Interrupt).await,
        Err(CodexErr::InternalAgentDied)
    ));
    assert!(matches!(
        outer.next_event().await,
        Err(CodexErr::InternalAgentDied)
    ));
    let (tx_bridge, rx_bridge) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let bridge = tokio::spawn(bridge_one_shot_events(
        outer,
        tx_bridge,
        CancellationToken::new(),
    ));
    timeout(Duration::from_secs(1), bridge)
        .await
        .expect("one-shot bridge did not observe proxy closure")
        .expect("one-shot bridge join error");
    assert!(matches!(
        rx_bridge.recv().await,
        Err(async_channel::RecvError)
    ));
    drop(tx_child_events);
}

#[tokio::test]
async fn failed_op_forwarding_closes_proxy_before_session_loop_termination() {
    let (tx_child_sub, rx_child_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_child_events, rx_child_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let (termination_tx, termination_rx) = oneshot::channel();
    let session_loop_termination =
        crate::session::session_loop_termination_from_handle(tokio::spawn(async move {
            let _ = termination_rx.await;
        }));
    let child = Arc::new(Codex {
        tx_sub: tx_child_sub,
        rx_event: rx_child_events,
        agent_status,
        session: Arc::clone(&session),
        session_loop_termination: session_loop_termination.clone(),
    });

    let (tx_outer_events, rx_outer_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_outer_ops, rx_outer_ops) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let delegate_liveness = CancellationToken::new();
    cancel_delegate_when_session_loop_terminates(&child, delegate_liveness.clone());
    let events = tokio::spawn(forward_events(
        Arc::clone(&child),
        tx_outer_events,
        Arc::clone(&session),
        ctx,
        delegate_liveness.clone(),
    ));
    let ops = tokio::spawn(forward_ops(
        Arc::clone(&child),
        rx_outer_ops,
        delegate_liveness,
    ));
    let outer = Codex {
        tx_sub: tx_outer_ops,
        rx_event: rx_outer_events,
        agent_status: child.agent_status.clone(),
        session,
        session_loop_termination,
    };

    drop(rx_child_sub);
    outer
        .submit(Op::Interrupt)
        .await
        .expect("outer proxy should accept the racing operation");
    timeout(Duration::from_secs(1), async {
        ops.await.expect("forward_ops join error");
        events.await.expect("forward_events join error");
    })
    .await
    .expect("delegate proxy did not close after failed op forwarding");

    assert!(matches!(
        outer.submit(Op::Interrupt).await,
        Err(CodexErr::InternalAgentDied)
    ));
    assert!(matches!(
        outer.next_event().await,
        Err(CodexErr::InternalAgentDied)
    ));
    termination_tx.send(()).expect("termination receiver alive");
    drop(tx_child_events);
}

#[tokio::test]
async fn run_codex_thread_interactive_respects_pre_cancelled_spawn() {
    let (parent_session, parent_ctx, _rx_events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = timeout(
        Duration::from_secs(/*secs*/ 1),
        run_codex_thread_interactive(
            parent_ctx.config.as_ref().clone(),
            Arc::clone(&parent_session.services.auth_manager),
            Arc::clone(&parent_session.services.models_manager),
            parent_session,
            parent_ctx,
            cancel_token,
            SubAgentSource::Review,
            /*initial_history*/ None,
        ),
    )
    .await
    .expect("cancelled delegate spawn should not hang");

    assert!(matches!(result, Err(CodexErr::TurnAborted)));
}

#[tokio::test]
async fn handle_request_permissions_uses_tool_call_id_for_round_trip() {
    let (parent_session, mut parent_ctx, rx_events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    *parent_session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        terminal: Some(crate::state::TurnTerminalCoordinator::new(
            parent_ctx.sub_id.clone(),
        )),
        ..Default::default()
    });
    let parent_ctx_mut = Arc::get_mut(&mut parent_ctx).expect("single turn context ref");
    parent_ctx_mut.environments.turn_environments[0].environment_id = "remote".to_string();

    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_tx_events, rx_events_child) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let codex = Arc::new(Codex {
        tx_sub,
        rx_event: rx_events_child,
        agent_status,
        session: Arc::clone(&parent_session),
        session_loop_termination: completed_session_loop_termination(),
    });

    let call_id = "tool-call-1".to_string();
    let expected_response = RequestPermissionsResponse {
        permissions: RequestPermissionProfile {
            network: Some(NetworkPermissions {
                enabled: Some(true),
            }),
            ..RequestPermissionProfile::default()
        },
        scope: PermissionGrantScope::Turn,
    };
    let delegated_cwd = parent_ctx.cwd().join("delegated-cwd");
    let cancel_token = CancellationToken::new();
    let request_call_id = call_id.clone();
    let request_cwd = delegated_cwd.clone();
    let request_cwd_uri = PathUri::from_abs_path(&delegated_cwd);

    let handle = tokio::spawn({
        let codex = Arc::clone(&codex);
        let parent_session = Arc::clone(&parent_session);
        let parent_ctx = Arc::clone(&parent_ctx);
        let cancel_token = cancel_token.clone();
        async move {
            handle_request_permissions(
                codex.as_ref(),
                &parent_session,
                &parent_ctx,
                RequestPermissionsEvent {
                    call_id: request_call_id,
                    turn_id: "child-turn-1".to_string(),
                    environment_id: Some("remote".to_string()),
                    started_at_ms: 0,
                    reason: Some("need access".to_string()),
                    permissions: RequestPermissionProfile {
                        network: Some(NetworkPermissions {
                            enabled: Some(true),
                        }),
                        ..RequestPermissionProfile::default()
                    },
                    cwd: Some(request_cwd),
                    cwd_uri: Some(request_cwd_uri),
                },
                &cancel_token,
            )
            .await;
        }
    });

    let request_event = timeout(Duration::from_secs(1), rx_events.recv())
        .await
        .expect("request_permissions event timed out")
        .expect("request_permissions event missing");
    let EventMsg::RequestPermissions(request) = request_event.msg else {
        panic!("expected RequestPermissions event");
    };
    assert_eq!(request.call_id, call_id.clone());
    assert_eq!(request.environment_id.as_deref(), Some("remote"));
    assert_eq!(request.cwd, Some(delegated_cwd));

    parent_session
        .notify_request_permissions_response(&call_id, expected_response.clone())
        .await;

    timeout(Duration::from_secs(1), handle)
        .await
        .expect("handle_request_permissions hung")
        .expect("handle_request_permissions join error");

    let submission = timeout(Duration::from_secs(1), rx_sub.recv())
        .await
        .expect("request_permissions response timed out")
        .expect("request_permissions response missing");
    assert_eq!(
        submission.submission.op,
        Op::RequestPermissionsResponse {
            id: call_id,
            response: expected_response,
        }
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the turn state reads borrow from their active-turn guards"
)]
async fn delegated_user_input_preserves_answers_and_reports_interruption() {
    for outcome in ["cancelled", "closed", "empty", "answered", "interrupted"] {
        let (parent_session, parent_ctx, rx_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        *parent_session.active_turn.lock().await = Some(crate::state::ActiveTurn {
            terminal: Some(crate::state::TurnTerminalCoordinator::new(
                parent_ctx.sub_id.clone(),
            )),
            ..Default::default()
        });
        let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_tx_events, rx_child_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
        let child = Codex {
            tx_sub,
            rx_event: rx_child_events,
            agent_status,
            session: Arc::clone(&parent_session),
            session_loop_termination: completed_session_loop_termination(),
        };

        let cancel_token = CancellationToken::new();
        let mut request = Box::pin(handle_request_user_input(
            &child,
            "child-input".to_string(),
            &parent_session,
            &parent_ctx,
            RequestUserInputEvent {
                call_id: "child-input".to_string(),
                turn_id: "child-turn".to_string(),
                questions: vec![RequestUserInputQuestion {
                    id: "next".to_string(),
                    header: "Next".to_string(),
                    question: "What next?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: None,
                }],
                auto_resolution_ms: None,
            },
            &cancel_token,
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        let event = timeout(Duration::from_secs(2), rx_events.recv())
            .await
            .expect("parent input request timed out")
            .expect("parent input request missing");
        let EventMsg::RequestUserInput(event) = event.msg else {
            panic!("expected parent user-input request");
        };
        assert_eq!(event.turn_id, parent_ctx.sub_id);
        assert_eq!(event.questions[0].id, "next");

        let expected = RequestUserInputResponse {
            answers: if outcome == "answered" {
                HashMap::from([(
                    "next".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Continue".to_string()],
                    },
                )])
            } else {
                HashMap::new()
            },
            interrupted: matches!(outcome, "cancelled" | "closed" | "interrupted"),
        };
        match outcome {
            "cancelled" => cancel_token.cancel(),
            "closed" => {
                let active = parent_session.active_turn.lock().await;
                let mut turn_state = active.as_ref().unwrap().turn_state.lock().await;
                assert!(
                    turn_state
                        .remove_pending_user_input(&parent_ctx.sub_id)
                        .is_some()
                );
            }
            _ => {
                parent_session
                    .notify_user_input_response(&parent_ctx.sub_id, expected.clone())
                    .await;
            }
        }
        timeout(Duration::from_secs(2), request)
            .await
            .expect("delegated input request hung");
        let submission = rx_sub.try_recv().expect("child input response missing");
        assert_eq!(
            submission.submission.op,
            Op::UserInputAnswer {
                id: "child-input".to_string(),
                response: expected,
            },
            "outcome: {outcome}"
        );
        assert!(rx_sub.try_recv().is_err(), "duplicate input response");
        let active = parent_session.active_turn.lock().await;
        assert!(
            !active
                .as_ref()
                .unwrap()
                .turn_state
                .lock()
                .await
                .has_pending_user_input(&parent_ctx.sub_id)
        );
    }
}

#[tokio::test]
async fn prepared_one_shot_cancels_blocked_output_and_preserves_terminal_delivery() {
    for stalled_receiver in [true, false] {
        let (session, ctx, _rx_evt) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (tx_child_sub, rx_child_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_child_events, rx_child_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
        let child = Arc::new(Codex {
            tx_sub: tx_child_sub,
            rx_event: rx_child_events,
            agent_status,
            session: Arc::clone(&session),
            session_loop_termination: completed_session_loop_termination(),
        });
        let (tx_outer_events, rx_outer_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let outer_events_observer = rx_outer_events.clone();
        let (tx_outer_ops, rx_outer_ops) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let parent_cancel = CancellationToken::new();
        let child_cancel = parent_cancel.child_token();
        let delegate_liveness = child_cancel.child_token();
        let events = tokio::spawn(forward_events(
            Arc::clone(&child),
            tx_outer_events,
            Arc::clone(&session),
            ctx,
            delegate_liveness.clone(),
        ));
        let ops = tokio::spawn(forward_ops(
            Arc::clone(&child),
            rx_outer_ops,
            delegate_liveness,
        ));
        // Replace only the external child process with channels. submit_once,
        // its output bridge and both interactive forwarders run unchanged.
        let prepared = PreparedCodexOneShot {
            io: Codex {
                tx_sub: tx_outer_ops,
                rx_event: rx_outer_events,
                agent_status: child.agent_status.clone(),
                session,
                session_loop_termination: completed_session_loop_termination(),
            },
            child_cancel: child_cancel.clone(),
            submitted: false,
        };
        let one_shot = prepared.submit_once(Vec::new(), None).await.unwrap();
        let submitted = timeout(Duration::from_secs(1), rx_child_sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(submitted.submission.op, Op::UserInput { .. }));
        assert!(one_shot.submit(Op::Interrupt).await.is_err());

        let terminal = Event {
            id: "one-shot-terminal".to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("one-shot-turn".to_string()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        };
        if stalled_receiver {
            for index in 0..SUBMISSION_CHANNEL_CAPACITY + 2 {
                tx_child_events
                    .send(Event {
                        id: format!("visible-{index}"),
                        msg: EventMsg::RawResponseItem(RawResponseItemEvent {
                            item: ResponseItem::CustomToolCall {
                                id: None,
                                status: None,
                                call_id: format!("call-{index}"),
                                name: "tool".to_string(),
                                namespace: None,
                                input: "{}".to_string(),
                                internal_chat_message_metadata_passthrough: None,
                            },
                        }),
                    })
                    .await
                    .unwrap();
            }
            timeout(Duration::from_secs(1), async {
                while one_shot.rx_event.len() != SUBMISSION_CHANNEL_CAPACITY
                    || outer_events_observer.is_empty()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("bridge never reached output backpressure");
            parent_cancel.cancel();
        } else {
            tx_child_events.send(terminal.clone()).await.unwrap();
            let delivered = timeout(Duration::from_secs(1), one_shot.next_event())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivered.id, "one-shot-terminal");
            let EventMsg::TurnAborted(delivered) = delivered.msg else {
                panic!("terminal event was not preserved");
            };
            assert_eq!(delivered.turn_id.as_deref(), Some("one-shot-turn"));
            assert!(matches!(delivered.reason, TurnAbortReason::Interrupted));
        }

        // Shutdown must reach the external child, not merely cancel the bridge.
        for expected_interrupt in [true, false] {
            let submission = timeout(Duration::from_secs(1), rx_child_sub.recv())
                .await
                .expect("child shutdown was not delivered")
                .unwrap();
            if expected_interrupt {
                assert!(matches!(submission.submission.op, Op::Interrupt));
            } else {
                assert!(matches!(submission.submission.op, Op::Shutdown));
            }
        }
        tx_child_events.send(terminal).await.unwrap();
        timeout(Duration::from_secs(1), async {
            events.await.unwrap();
            ops.await.unwrap();
            while !one_shot.rx_event.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one-shot bridge or child forwarder survived shutdown");
        assert!(child_cancel.is_cancelled());
        if stalled_receiver {
            assert_eq!(one_shot.rx_event.len(), SUBMISSION_CHANNEL_CAPACITY);
        } else {
            assert!(one_shot.next_event().await.is_err());
        }
    }
}
