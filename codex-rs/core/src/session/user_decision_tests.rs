use super::*;
use crate::context::ContextualUserFragment;
use crate::plan_store::plan_step_id;
use crate::state::ActiveTurn;
use crate::state::TurnTerminalCoordinator;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::RequestUserInputHandler;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use serde_json::json;
use tokio::sync::Mutex;

fn request() -> RequestUserInputArgs {
    serde_json::from_value(json!({"questions":[{"id":"scope","header":"Scope","question":"Should deployment remain in scope?",
        "options":[{"label":"Review only","description":"Do not deploy; provide the review report only."},
        {"label":"Deploy","description":"Deploy after reviewing."}]}]})).unwrap()
}

fn answer() -> RequestUserInputResponse {
    serde_json::from_value(json!({"answers":{"scope":{"answers":["Review only"]}},"interrupted":false,"disposition":"answered"})).unwrap()
}

async fn seed_plan(session: &Session) {
    session
        .services
        .plan_store
        .stage_tool(
            serde_json::from_value(json!({"plan":[
        {"step":"Review","status":"pending"},{"step":"Deploy","status":"pending"}]}))
            .unwrap(),
        )
        .await
        .unwrap()
        .commit_published();
}

async fn retire(session: &Session, reference: serde_json::Value) -> Result<(), String> {
    let store = &session.services.plan_store;
    let revision = store.execution_snapshot().await.unwrap().revision;
    let id = plan_step_id("Deploy");
    store
        .stage_tool(
            serde_json::from_value(json!({"expected_revision":revision,
        "plan":[{"step":"Review","status":"pending"}],
        "superseded":[{"step_id":id,"reason":"User chose review only"}],
        "scope_change_refs":{id:reference}}))
            .unwrap(),
        )
        .await
        .map(|staged| {
            staged.commit_published();
        })
}

#[tokio::test]
async fn matched_ui_answer_authenticates_plan_retirement_through_real_handler() {
    let (session, turn, events) = super::super::tests::make_session_and_context_with_rx().await;
    seed_plan(&session).await;
    *session.active_turn.lock().await = Some(ActiveTurn {
        terminal: Some(TurnTerminalCoordinator::new(turn.sub_id.clone())),
        ..Default::default()
    });
    let mut invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "scope-call".into(),
        tool_name: codex_tools::ToolName::plain("request_user_input"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::to_string(&request()).unwrap(),
        },
    };
    // Exercise repaired, normalized presentation context, not just raw arguments.
    let mut raw = serde_json::to_value(request()).unwrap();
    raw["questions"][0]["header"] = json!("Deployment decision context");
    invocation.payload = ToolPayload::Function {
        arguments: raw.to_string(),
    };
    let handler = RequestUserInputHandler {
        available_modes: vec![session.collaboration_mode().await.mode],
    };
    let mut task = Box::pin(handler.handle(invocation));
    let emitted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                result = &mut task => panic!("request completed before answer: {:?}", result.err()),
                event = events.recv() => if let EventMsg::RequestUserInput(event) = event.unwrap().msg { break event; },
            }
        }
    }).await.unwrap();
    assert!(
        emitted.questions[0]
            .question
            .starts_with("Deployment decision context\n\n")
    );
    // Foreign call IDs must neither consume the waiter nor produce authority.
    session
        .notify_user_input_response_for_request(&turn.sub_id, Some("wrong-call"), answer())
        .await;
    assert!(
        !session
            .services
            .plan_store
            .task_state(false)
            .await
            .body()
            .contains("accepted_input_refs")
    );
    session
        .notify_user_input_response_for_request(&turn.sub_id, Some("scope-call"), answer())
        .await;
    let output = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output.code_mode_result(&ToolPayload::Function {
            arguments: raw.to_string()
        }),
        serde_json::to_value(answer()).unwrap(),
    );
    let normalized = RequestUserInputArgs {
        questions: emitted.questions,
        auto_resolution_ms: emitted.auto_resolution_ms,
    };
    let decision = AcceptedUserDecision::new(
        session.thread_id,
        &turn.sub_id,
        "scope-call",
        normalized,
        &answer(),
    )
    .unwrap();
    let reference =
        serde_json::to_value(&decision.accepted_input().unwrap().references()[0]).unwrap();
    let state = session
        .services
        .plan_store
        .task_state(false)
        .await
        .body()
        .into_owned();
    assert!(state.contains(reference["sha256"].as_str().unwrap()));
    let mut foreign = reference.clone();
    foreign["turn_id"] = json!("foreign");
    assert!(retire(&session, foreign).await.is_err());
    assert!(retire(&session, reference).await.is_ok());
    let (plan, lineage) = session
        .services
        .plan_store
        .snapshot_with_lineage()
        .await
        .unwrap();
    assert_eq!(plan.plan.len(), 1);
    assert_eq!(plan.plan[0].step, "Review");
    assert_eq!(lineage.obligation_summary(&plan).unresolved.len(), 1);
    session.terminal_tasks.close();
    tokio::time::timeout(Duration::from_secs(3), session.terminal_tasks.wait())
        .await
        .unwrap();
}

#[tokio::test]
async fn decision_snapshot_replays_without_prompt_injection_and_rollback_removes_it() {
    let (session, turn) = super::super::tests::make_session_and_context().await;
    let decision = AcceptedUserDecision::new(
        session.thread_id,
        &turn.sub_id,
        "scope-call",
        request(),
        &answer(),
    )
    .unwrap();
    let snapshot = decision.snapshot().unwrap();
    let mut items = vec![
        RolloutItem::TurnContext(turn.to_turn_context_item()),
        RolloutItem::ResponseItem(snapshot.clone()),
    ];
    let rebuilt = session
        .reconstruct_history_from_rollout(&turn, &items)
        .await;
    assert_eq!(rebuilt.accepted_input, decision.accepted_input());
    assert!(
        !rebuilt
            .history
            .iter()
            .any(AcceptedUserDecision::is_snapshot)
    );
    // An untrusted user/tool-shaped copy and another session are not witnesses.
    assert!(AcceptedUserDecision::from_snapshot(&snapshot, ThreadId::new()).is_none());
    let mut forged = snapshot.clone();
    if let ResponseItem::Message { role, .. } = &mut forged {
        *role = "user".into();
    }
    let fake = session
        .reconstruct_history_from_rollout(&turn, &[RolloutItem::ResponseItem(forged)])
        .await;
    assert!(fake.accepted_input.is_none());
    items.insert(
        0,
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                message: "Review and deploy".into(),
                ..Default::default()
            },
        )),
    );
    items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
    )));
    let rebuilt = session
        .reconstruct_history_from_rollout(&turn, &items)
        .await;
    assert!(rebuilt.accepted_input.is_none());
}

#[tokio::test]
async fn nonanswers_cancellation_and_failed_persistence_cannot_mint_decision_authority() {
    let (session, turn, _events) = super::super::tests::make_session_and_context_with_rx().await;
    seed_plan(&session).await;
    let mut replies = Vec::new();
    for disposition in ["timed_out", "skipped", "interrupted", "transport_error"] {
        let mut response = serde_json::to_value(answer()).unwrap();
        response["disposition"] = json!(disposition);
        replies.push(serde_json::from_value(response).unwrap());
    }
    let mut interrupted = answer();
    interrupted.interrupted = true;
    replies.push(interrupted);
    let mut legacy = answer();
    legacy.disposition = None;
    replies.push(legacy);
    let mut empty = answer();
    empty.answers.clear();
    replies.push(empty);
    let mut whitespace = answer();
    whitespace.answers.get_mut("scope").unwrap().answers = vec![" ".into()];
    replies.push(whitespace);
    for response in replies {
        session
            .record_accepted_user_decision(
                &turn,
                "scope-call",
                request(),
                &response,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !session
                .services
                .plan_store
                .task_state(false)
                .await
                .body()
                .contains("accepted_input_refs")
        );
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        session
            .record_accepted_user_decision(&turn, "scope-call", request(), &answer(), cancellation)
            .await
            .is_err()
    );
    let decision = AcceptedUserDecision::new(
        session.thread_id,
        &turn.sub_id,
        "scope-call",
        request(),
        &answer(),
    )
    .unwrap();
    assert!(
        session
            .services
            .plan_store
            .record_accepted_decision(decision.accepted_input().unwrap(), async {
                Err(std::io::Error::other("injected storage failure"))
            })
            .await
            .is_err()
    );
    assert!(
        !session
            .services
            .plan_store
            .task_state(false)
            .await
            .body()
            .contains("accepted_input_refs")
    );
    // A blocked publication holds no authority until persistence completes and
    // prevents a concurrent scope update from racing ahead of the input commit.
    let (release, blocked) = oneshot::channel::<()>();
    let record = session
        .services
        .plan_store
        .record_accepted_decision(decision.accepted_input().unwrap(), async {
            blocked.await.map_err(std::io::Error::other)
        });
    tokio::pin!(record);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut record)
            .await
            .is_err()
    );
    let reference =
        serde_json::to_value(&decision.accepted_input().unwrap().references()[0]).unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            retire(&session, reference.clone())
        )
        .await
        .is_err()
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), record)
        .await
        .unwrap()
        .unwrap();
    assert!(
        session
            .services
            .plan_store
            .task_state(false)
            .await
            .body()
            .contains("accepted_input_refs")
    );
    assert!(retire(&session, reference).await.is_ok());
}

#[test]
fn decision_digest_binds_question_options_call_session_and_complete_unicode_answer() {
    let session_id = ThreadId::new();
    let baseline =
        AcceptedUserDecision::new(session_id, "turn", "call", request(), &answer()).unwrap();
    let mut other_question = request();
    other_question.questions[0].question = "Different scope?".into();
    let mut other_option = request();
    other_option.questions[0].options.as_mut().unwrap()[0].description = "Deploy anyway".into();
    let mut unicode = answer();
    unicode.answers.get_mut("scope").unwrap().answers = vec!["Review only\r\n不要部署".into()];
    for decision in [
        AcceptedUserDecision::new(session_id, "turn", "call", other_question, &answer()).unwrap(),
        AcceptedUserDecision::new(session_id, "turn", "call", other_option, &answer()).unwrap(),
        AcceptedUserDecision::new(session_id, "turn", "other-call", request(), &answer()).unwrap(),
        AcceptedUserDecision::new(ThreadId::new(), "turn", "call", request(), &answer()).unwrap(),
        AcceptedUserDecision::new(session_id, "turn", "call", request(), &unicode).unwrap(),
    ] {
        assert_ne!(baseline.accepted_input(), decision.accepted_input());
    }
    let snapshot = baseline.snapshot().unwrap();
    assert_eq!(
        AcceptedUserDecision::from_snapshot(&snapshot, session_id)
            .unwrap()
            .canonical_text()
            .unwrap(),
        baseline.canonical_text().unwrap()
    );
}
