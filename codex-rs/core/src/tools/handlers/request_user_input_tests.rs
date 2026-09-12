use super::*;
use crate::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

fn request_invocation(
    session: Arc<Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(turn),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({
                "questions": [{
                    "header": "Hdr",
                    "question": "Pick one",
                    "id": "pick_one",
                    "options": [
                        {
                            "label": "A",
                            "description": "A"
                        },
                        {
                            "label": "B",
                            "description": "B"
                        }
                    ]
                }]
            })
            .to_string(),
        },
    }
}

#[tokio::test]
async fn multi_agent_v2_request_user_input_rejects_subagent_threads() {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let turn = Arc::new(turn);

    let result = RequestUserInputHandler {
        available_modes: Vec::new(),
    }
    .handle(request_invocation(Arc::new(session), Arc::clone(&turn)))
    .await;

    let Err(err) = result else {
        panic!("sub-agent request_user_input should fail");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "request_user_input can only be used by the root thread".to_string(),
        )
    );
}

#[tokio::test]
async fn never_approval_request_user_input_returns_recoverable_error() {
    let (session, mut turn) = make_session_and_context().await;
    turn.approval_policy = codex_config::Constrained::allow_any(AskForApproval::Never);
    let turn = Arc::new(turn);

    let result = RequestUserInputHandler {
        available_modes: Vec::new(),
    }
    .handle(request_invocation(Arc::new(session), turn))
    .await;

    let Err(err) = result else {
        panic!("never-approval request_user_input should fail recoverably");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "request_user_input is unavailable when approval policy is `never`; continue without interactive input or return a final response explaining the missing information"
                .to_string(),
        )
    );
}

async fn registered_input_request(
    session: Arc<Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
) -> Result<crate::tools::registry::AnyToolResult, FunctionCallError> {
    use crate::tools::context::{ToolCallSource, ToolDispatchState};
    use crate::tools::router::{ToolCall, ToolRouter, ToolRouterParams};
    let invocation = request_invocation(Arc::clone(&session), Arc::clone(&turn));
    let step = StepContext::for_test(turn);
    let router = ToolRouter::from_context(
        step.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );
    // This registered router boundary receives an already-admitted dispatch
    // from ToolCallRuntime; mirror the existing registry test fixture state.
    let dispatch_state = Arc::new(ToolDispatchState::new());
    assert!(dispatch_state.try_admit());
    router
        .dispatch_tool_call_with_terminal_outcome(
            session,
            step,
            tokio_util::sync::CancellationToken::new(),
            Arc::new(Mutex::new(TurnDiffTracker::new())),
            ToolCall {
                tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
                call_id: call_id.to_string(),
                payload: invocation.payload,
            },
            ToolCallSource::Direct,
            dispatch_state,
        )
        .await
}

#[tokio::test]
async fn registered_user_input_drop_retires_sender_before_and_after_event_delivery() {
    use crate::state::{ActiveTurn, TurnTerminalCoordinator};
    use codex_protocol::protocol::{Event, EventMsg, WarningEvent};
    use std::time::Duration;
    for blocked in [true, false] {
        let (session, turn, events_tx, events) =
            crate::session::tests::make_session_and_context_with_event_capacity(if blocked {
                1
            } else {
                64
            })
            .await;
        let active = ActiveTurn {
            terminal: Some(TurnTerminalCoordinator::new(turn.sub_id.clone())),
            ..Default::default()
        };
        let original = Arc::clone(&active.turn_state);
        *session.active_turn.lock().await = Some(active);
        if blocked {
            events_tx
                .send(Event {
                    id: "sentinel".into(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: "occupied".into(),
                    }),
                })
                .await
                .unwrap();
        }
        let mut request = Box::pin(registered_input_request(
            Arc::clone(&session),
            Arc::clone(&turn),
            "drop-input",
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                result = &mut request => panic!("request must await response; error={:?}", result.err()),
                () = async { while !original.lock().await.has_pending_user_input(&turn.sub_id) { tokio::task::yield_now().await; } } => {}
            }
        }).await.expect("registered request reaches pending sender");
        if !blocked {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    tokio::select! {
                        result = &mut request => panic!("request completed before response; error={:?}", result.err()),
                        event = events.recv() => {
                            if let EventMsg::RequestUserInput(event) = event.unwrap().msg {
                                assert_eq!(event.call_id, "drop-input");
                                assert_eq!(event.turn_id, turn.sub_id);
                                assert_eq!(event.questions[0].id, "pick_one");
                                break;
                            }
                        }
                    }
                }
            }).await.expect("normal user request is delivered");
        }
        // A new turn may already own the session when the old waiter is dropped.
        *session.active_turn.lock().await = Some(ActiveTurn {
            terminal: Some(TurnTerminalCoordinator::new("new-active-turn".into())),
            ..Default::default()
        });
        drop(request);
        session.terminal_tasks.close();
        tokio::time::timeout(Duration::from_secs(3), session.terminal_tasks.wait())
            .await
            .expect("drop cleanup completes");
        assert!(!original.lock().await.has_pending_user_input(&turn.sub_id));
        if blocked {
            assert_eq!(events.recv().await.unwrap().id, "sentinel");
            assert!(
                events.try_recv().is_err(),
                "dropped request cannot emit later"
            );
        }
    }
}

#[tokio::test]
async fn registered_user_input_rejects_stale_turn_and_preserves_live_replacement() {
    use crate::state::{ActiveTurn, TurnTerminalCoordinator};
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::request_user_input::RequestUserInputResponse;
    use std::time::Duration;
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let active = ActiveTurn {
        terminal: Some(TurnTerminalCoordinator::new("replacement-turn".into())),
        ..Default::default()
    };
    let state = Arc::clone(&active.turn_state);
    *session.active_turn.lock().await = Some(active);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        registered_input_request(Arc::clone(&session), Arc::clone(&turn), "stale-input"),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "stale caller cannot await a new turn's reply"
    );
    assert!(!state.lock().await.has_pending_user_input(&turn.sub_id));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::RequestUserInput(_)),
            "stale input must not prompt the user"
        );
    }
    *session.active_turn.lock().await = Some(ActiveTurn {
        terminal: Some(TurnTerminalCoordinator::new(turn.sub_id.clone())),
        ..Default::default()
    });
    let state = Arc::clone(
        &session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .turn_state,
    );
    let mut first = Box::pin(registered_input_request(
        Arc::clone(&session),
        Arc::clone(&turn),
        "old-input",
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                result = &mut first => panic!("first request completed; error={:?}", result.err()),
                event = events.recv() => { if matches!(event.unwrap().msg, EventMsg::RequestUserInput(_)) { break; } }
            }
        }
    }).await.unwrap();
    let mut replacement = Box::pin(registered_input_request(
        Arc::clone(&session),
        Arc::clone(&turn),
        "new-input",
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                result = &mut replacement => panic!("replacement completed; error={:?}", result.err()),
                event = events.recv() => { if let EventMsg::RequestUserInput(event) = event.unwrap().msg { assert_eq!(event.call_id, "new-input"); break; } }
            }
        }
    }).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), first)
            .await
            .expect("replacement releases previous waiter promptly")
            .is_err(),
        "same-turn replacement retires the previous response waiter"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while session.terminal_tasks.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        state.lock().await.has_pending_user_input(&turn.sub_id),
        "old waiter cleanup preserves live same-key replacement"
    );
    let response: RequestUserInputResponse = serde_json::from_value(
        json!({"answers":{"pick_one":{"answers":["B"]}},"interrupted":false}),
    )
    .unwrap();
    session
        .notify_user_input_response(&turn.sub_id, response.clone())
        .await;
    let result = replacement
        .await
        .expect("matching response reaches registered handler");
    let model = result
        .result
        .to_response_item(&result.call_id, &result.payload);
    let encoded = serde_json::to_value(model).unwrap();
    let text = encoded
        .get("output")
        .and_then(serde_json::Value::as_str)
        .expect("function output text");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        serde_json::to_value(response).unwrap()
    );
    assert!(!state.lock().await.has_pending_user_input(&turn.sub_id));
    session.terminal_tasks.close();
    tokio::time::timeout(Duration::from_secs(3), session.terminal_tasks.wait())
        .await
        .unwrap();
}
